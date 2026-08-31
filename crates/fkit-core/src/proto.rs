//! The sync protocol.
//!
//! # Why a Merkle DAG makes sync easy
//!
//! To synchronise two repositories you must answer "what do you have that I
//! don't?" — a set reconciliation problem that is expensive in general. Sending
//! a full inventory is O(repository); binary-searching history is fiddly and
//! only works for linear logs.
//!
//! A Merkle DAG collapses the problem. Every node's hash covers its entire
//! subtree, so:
//!
//! > **If we both have hash X, we both have everything underneath X.**
//!
//! Nothing else needs checking. So sync becomes a trivial graph walk:
//!
//! ```text
//!   1. ask for the tip commit
//!   2. for each object received, look at the hashes it references
//!   3. request only the ones you don't already have
//!   4. repeat until nothing is missing
//! ```
//!
//! An unchanged directory is one hash comparison, and the entire subtree beneath
//! it is skipped — not transferred, not enumerated, not even named. Pushing a
//! one-line fix to a repo with a million files transfers a handful of objects.
//! This is the same mechanism `fkit diff` uses locally, run over a socket.
//!
//! # Trust
//!
//! The receiver names every object it wants by hash, and [`Store::put_raw`]
//! recomputes that hash before storing. A malicious peer therefore cannot
//! substitute content: the worst it can do is refuse to answer or send garbage
//! that gets rejected. Verifying the tip commit hash transitively verifies every
//! byte beneath it.

use crate::hash::Hash;
use crate::store::Store;
use crate::ws::WebSocket;
use anyhow::{bail, Context, Result};
use std::collections::{HashSet, VecDeque};

/// A bidirectional message channel the sync protocol can run over.
///
/// The negotiation below is pure logic — it does not care whether messages
/// travel over a blocking [`WebSocket`], an async socket bridged through a
/// channel, or an in-memory pipe in a test. Keeping it abstract is what lets the
/// same `fetch_closure`/`serve_wants` pair serve the standalone `fkitd` daemon
/// (blocking threads) and the axum-based hub (async tasks) without a second
/// implementation to keep in sync.
pub trait Transport {
    fn send_bytes(&mut self, payload: &[u8]) -> Result<()>;
    /// `Ok(None)` means the peer closed cleanly.
    fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>>;
}

impl Transport for WebSocket {
    fn send_bytes(&mut self, payload: &[u8]) -> Result<()> {
        self.send(payload)
    }
    fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        self.recv()
    }
}

/// Objects requested per round trip. Bounds the size of any single message
/// while keeping the number of round trips low.
/// Objects asked for in one round trip.
///
/// The exchange is strictly request-and-reply, so every batch is a full stop:
/// the sender is idle while the receiver writes and vice versa. At 256 a push
/// of a large history spends thousands of those stops doing nothing. Raising
/// it costs a larger message in flight and saves the round trips that were
/// most of the wall clock.
pub const BATCH: usize = 4096;

/// How many bytes of objects one reply message may carry.
///
/// `BATCH` bounds a reply in objects; this bounds it in bytes, which is the
/// thing a receiver actually has a limit on. Without it a batch of large
/// objects built a message past what the other end would accept, and a
/// websocket peer that refuses a frame does not say so -- it closes, and both
/// sides report only that the other one went away.
///
/// Chosen to sit under the smallest limit anything in this system imposes
/// (axum's 16 MiB default frame) with room to spare, while still being large
/// enough that the round trips `BATCH` was raised to save stay saved.
pub const REPLY_BUDGET: usize = 8 * 1024 * 1024;

/// The most one reply may hold before it is treated as a peer that will not
/// stop talking. A whole batch of large objects, with room to spare.
const REPLY_LIMIT: usize = 512 * 1024 * 1024;

/// How many times a transfer may go back for objects an earlier one lost.
///
/// A handful. Each round fills the gaps the previous one revealed, and a
/// history broken more deeply than that wants looking at rather than retrying.
const MAX_REPAIR_ROUNDS: usize = 8;

/// Refuse to follow an unbounded graph from an untrusted peer.
pub const MAX_OBJECTS_PER_SYNC: usize = 5_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Client opens: which repository, and an optional auth token.
    Hello { repo: String, token: String },
    /// Server accepts, and states everything it currently has.
    Welcome { refs: Vec<(String, Hash)> },

    /// "I am about to give you this branch tip."
    PushRef { branch: String, tip: Hash, force: bool },

    // ---- stashes ----
    //
    // Work parked on the server so it can be picked up on another machine.
    // Every other push attaches a ref, because until now everything a server
    // was asked to keep belonged to the history some branch or tag pointed at.
    // A stash is kept and is deliberately not in the ref namespace, so it needs
    // its own verbs: the objects arrive, and what keeps them alive is recorded
    // beside them rather than as a name anyone else can see.

    /// Send the closure of `tip` and park it under this account.
    PushStash { tip: Hash, message: String },
    /// What this account has parked here.
    ListStashes,
    StashList { entries: Vec<(Hash, String)> },
    /// Ask for one back. The server serves the closure as a pull does.
    PullStash { commit: Hash },
    DropStash { commit: Hash },
    /// "Tell me about this branch."
    PullRef { branch: String },
    RefIs { branch: String, tip: Option<Hash> },

    /// "Send me these objects." The core of the negotiation.
    Want { hashes: Vec<Hash> },
    /// The answer: framed object bytes, each verifiable against its hash.
    Objects { objects: Vec<(Hash, Vec<u8>)> },
    /// The same answer, for objects the sender already holds as a patch.
    ///
    /// Each payload names its own base and expands to the object; the base is
    /// sent as a literal first, in the same reply, so the far end can always
    /// apply it. Sending the expansion instead meant a store that had gone to
    /// the trouble of patching an object handed over the whole of it anyway,
    /// and the receiving store, told nothing about the relationship, wrote it
    /// out whole as well.
    Patches { items: Vec<(Hash, u32, Vec<u8>)> },
    /// "I have everything I need."
    Done,

    Ok { message: String },
    Error { message: String },
}

// ---- wire encoding ------------------------------------------------------

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn hash(&mut self, h: Hash) { self.0.extend_from_slice(&h.0) }
    fn bytes(&mut self, b: &[u8]) { self.u32(b.len() as u32); self.0.extend_from_slice(b) }
    fn str(&mut self, s: &str) { self.bytes(s.as_bytes()) }
}

struct R<'a> { b: &'a [u8], i: usize }
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.i + n > self.b.len() {
            bail!("truncated message");
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> { Ok(self.take(1)?[0]) }
    fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn hash(&mut self) -> Result<Hash> { Ok(Hash(self.take(32)?.try_into().unwrap())) }
    fn bytes(&mut self) -> Result<Vec<u8>> { let n = self.u32()? as usize; Ok(self.take(n)?.to_vec()) }
    fn str(&mut self) -> Result<String> { Ok(String::from_utf8(self.bytes()?)?) }
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Msg::Hello { repo, token } => { w.u8(0x01); w.str(repo); w.str(token) }
            Msg::Welcome { refs } => {
                w.u8(0x02);
                w.u32(refs.len() as u32);
                for (n, h) in refs { w.str(n); w.hash(*h) }
            }
            Msg::PushRef { branch, tip, force } => {
                w.u8(0x10); w.str(branch); w.hash(*tip); w.u8(*force as u8)
            }
            Msg::PushStash { tip, message } => { w.u8(0x13); w.hash(*tip); w.str(message) }
            Msg::ListStashes => w.u8(0x14),
            Msg::StashList { entries } => {
                w.u8(0x15);
                w.u32(entries.len() as u32);
                for (h, m) in entries { w.hash(*h); w.str(m) }
            }
            Msg::PullStash { commit } => { w.u8(0x16); w.hash(*commit) }
            Msg::DropStash { commit } => { w.u8(0x17); w.hash(*commit) }
            Msg::PullRef { branch } => { w.u8(0x11); w.str(branch) }
            Msg::RefIs { branch, tip } => {
                w.u8(0x12); w.str(branch);
                match tip { Some(h) => { w.u8(1); w.hash(*h) } None => w.u8(0) }
            }
            Msg::Want { hashes } => {
                w.u8(0x20);
                w.u32(hashes.len() as u32);
                for h in hashes { w.hash(*h) }
            }
            Msg::Objects { objects } => {
                w.u8(0x21);
                w.u32(objects.len() as u32);
                for (h, b) in objects { w.hash(*h); w.bytes(b) }
            }
            Msg::Patches { items } => {
                w.u8(0x23);
                w.u32(items.len() as u32);
                for (h, raw, b) in items {
                    w.hash(*h);
                    w.u32(*raw);
                    w.bytes(b)
                }
            }
            Msg::Done => w.u8(0x22),
            Msg::Ok { message } => { w.u8(0x30); w.str(message) }
            Msg::Error { message } => { w.u8(0x31); w.str(message) }
        }
        w.0
    }

    pub fn decode(buf: &[u8]) -> Result<Msg> {
        let mut r = R { b: buf, i: 0 };
        Ok(match r.u8()? {
            0x01 => Msg::Hello { repo: r.str()?, token: r.str()? },
            0x02 => {
                let n = r.u32()? as usize;
                let mut refs = Vec::with_capacity(n);
                for _ in 0..n { refs.push((r.str()?, r.hash()?)) }
                Msg::Welcome { refs }
            }
            0x10 => Msg::PushRef { branch: r.str()?, tip: r.hash()?, force: r.u8()? != 0 },
            0x11 => Msg::PullRef { branch: r.str()? },
            0x13 => Msg::PushStash { tip: r.hash()?, message: r.str()? },
            0x14 => Msg::ListStashes,
            0x15 => {
                let n = r.u32()? as usize;
                let mut entries = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    entries.push((r.hash()?, r.str()?));
                }
                Msg::StashList { entries }
            }
            0x16 => Msg::PullStash { commit: r.hash()? },
            0x17 => Msg::DropStash { commit: r.hash()? },
            0x12 => {
                let branch = r.str()?;
                let tip = if r.u8()? == 1 { Some(r.hash()?) } else { None };
                Msg::RefIs { branch, tip }
            }
            0x20 => {
                let n = r.u32()? as usize;
                let mut hashes = Vec::with_capacity(n.min(BATCH * 4));
                for _ in 0..n { hashes.push(r.hash()?) }
                Msg::Want { hashes }
            }
            0x21 => {
                let n = r.u32()? as usize;
                let mut objects = Vec::with_capacity(n.min(BATCH * 4));
                for _ in 0..n { objects.push((r.hash()?, r.bytes()?)) }
                Msg::Objects { objects }
            }
            0x23 => {
                let n = r.u32()? as usize;
                let mut items = Vec::with_capacity(n.min(BATCH * 4));
                for _ in 0..n { items.push((r.hash()?, r.u32()?, r.bytes()?)) }
                Msg::Patches { items }
            }
            0x22 => Msg::Done,
            0x30 => Msg::Ok { message: r.str()? },
            0x31 => Msg::Error { message: r.str()? },
            other => bail!("unknown protocol message type 0x{other:02X}"),
        })
    }
}

impl Msg {
    /// What kind of message this is, without its contents.
    ///
    /// An error that says what it received should not print an entire history
    /// at somebody: a `Want` carries thousands of hashes, and a mismatch used
    /// to spell every one of them out on the terminal.
    pub fn name(&self) -> &'static str {
        match self {
            Msg::Hello { .. } => "Hello",
            Msg::Welcome { .. } => "Welcome",
            Msg::PushRef { .. } => "PushRef",
            Msg::PushStash { .. } => "PushStash",
            Msg::ListStashes => "ListStashes",
            Msg::StashList { .. } => "StashList",
            Msg::PullStash { .. } => "PullStash",
            Msg::DropStash { .. } => "DropStash",
            Msg::PullRef { .. } => "PullRef",
            Msg::RefIs { .. } => "RefIs",
            Msg::Want { .. } => "Want",
            Msg::Objects { .. } => "Objects",
            Msg::Patches { .. } => "Patches",
            Msg::Done => "Done",
            Msg::Ok { .. } => "Ok",
            Msg::Error { .. } => "Error",
        }
    }
}

pub fn send<T: Transport + ?Sized>(t: &mut T, m: &Msg) -> Result<()> {
    t.send_bytes(&m.encode())
}

pub fn recv<T: Transport + ?Sized>(t: &mut T) -> Result<Msg> {
    let bytes = t.recv_bytes()?.context("peer disconnected")?;
    let m = Msg::decode(&bytes)?;
    if let Msg::Error { message } = &m {
        bail!("remote error: {message}");
    }
    Ok(m)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TransferStats {
    pub objects: u64,
    pub bytes: u64,
    pub round_trips: u64,
}

impl TransferStats {
    pub fn merge(&mut self, other: &TransferStats) {
        self.objects += other.objects;
        self.bytes += other.bytes;
        self.round_trips += other.round_trips;
    }
}

/// **Receiving half.** Walk the DAG from `roots`, requesting only what is
/// missing, until the local store holds the complete closure.
///
/// The peer is running [`serve_wants`] at the same time.
pub fn fetch_closure<T: Transport + ?Sized>(
    store: &Store,
    ws: &mut T,
    roots: &[Hash],
) -> Result<TransferStats> {
    fetch_closure_watched(store, ws, roots, &mut |_| {})
}

/// As [`fetch_closure`], reporting as it goes.
///
/// A transfer of a large history runs for minutes. Without this it prints
/// nothing at all until it finishes, which is indistinguishable from being
/// stuck -- and the one time it really was stuck, that is exactly how it
/// looked.
/// Where a transfer's time goes, when `FKIT_TIMING` is set. Diagnostic only.
pub mod timing {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static RECV: AtomicU64 = AtomicU64::new(0);
    pub static HAS: AtomicU64 = AtomicU64::new(0);
    pub static PUT: AtomicU64 = AtomicU64::new(0);
    pub static LINKS: AtomicU64 = AtomicU64::new(0);
    pub static WALK: AtomicU64 = AtomicU64::new(0);
    pub static PRUNES: AtomicU64 = AtomicU64::new(0);
    /// Queue depth left behind after a batch is taken, summed over batches.
    pub static LEFTOVER: AtomicU64 = AtomicU64::new(0);
    pub static BATCHES: AtomicU64 = AtomicU64::new(0);
    /// Server side.
    pub static SRV_READ: AtomicU64 = AtomicU64::new(0);
    pub static SRV_SEND: AtomicU64 = AtomicU64::new(0);

    pub fn on() -> bool {
        std::env::var_os("FKIT_TIMING").is_some()
    }

    pub fn add(c: &AtomicU64, t: std::time::Instant) {
        c.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn report() -> String {
        let ms = |c: &AtomicU64| c.load(Ordering::Relaxed) as f64 / 1e6;
        format!(
            "recv {:.0}ms  has {:.0}ms  put {:.0}ms  links {:.0}ms  gapwalk {:.0}ms  \
             prunes {}  leftover {}/{} batches  srv_read {:.0}ms  srv_send {:.0}ms",
            ms(&RECV), ms(&HAS), ms(&PUT), ms(&LINKS), ms(&WALK),
            PRUNES.load(Ordering::Relaxed),
            LEFTOVER.load(Ordering::Relaxed),
            BATCHES.load(Ordering::Relaxed),
            ms(&SRV_READ), ms(&SRV_SEND)
        )
    }
}

/// One object as it came off the wire, before the store has seen it.
enum Incoming {
    Whole(Hash, Vec<u8>),
    Patched(Hash, u32, Vec<u8>),
}

/// The next request's worth of hashes, and what that leaves behind.
fn take_batch(queue: &mut VecDeque<Hash>) -> Vec<Hash> {
    let batch: Vec<Hash> = (0..BATCH).filter_map(|_| queue.pop_front()).collect();
    if !batch.is_empty() {
        timing::LEFTOVER.fetch_add(queue.len() as u64, std::sync::atomic::Ordering::Relaxed);
        timing::BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    batch
}

pub fn fetch_closure_watched<T: Transport + ?Sized>(
    store: &Store,
    ws: &mut T,
    roots: &[Hash],
    progress: &mut dyn FnMut(&TransferStats),
) -> Result<TransferStats> {
    let mut stats = TransferStats::default();
    let mut queue: VecDeque<Hash> = VecDeque::new();
    let mut requested: HashSet<Hash> = HashSet::new();

    // Whether the walk ever stopped at something that was already here.
    //
    // Gaps are only possible underneath such a thing: everything fetched on
    // this connection had its links followed, so its children were either
    // fetched too or were already present and pruned at in turn. A transfer
    // that never pruned has therefore covered its whole closure, and the scan
    // for holes at the end has nothing it could find -- on a fresh clone it
    // was a second full pass over every object, for nothing.
    let mut pruned = false;

    for &r in roots {
        if store.has(r) {
            pruned = true;
            timing::PRUNES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else if requested.insert(r) {
            queue.push_back(r);
        }
    }

    // The walk and the gap-filling are one loop, not two.
    //
    // The walk prunes at anything already here, which is what makes an
    // incremental transfer cheap -- and also what makes it skip the gaps a
    // previously interrupted transfer left, since the objects above a gap are
    // present and get pruned at. So when the queue empties, look for holes and
    // put them back into the same queue: whatever comes back then has its own
    // links followed, the way everything else does.
    //
    // Fetching gaps in a loop of their own instead advanced by one layer of
    // the graph per round, because nothing followed the links of what it
    // fetched. A chain of parent commits is as deep as the history is long,
    // so a store missing one in the middle needed a round per commit and gave
    // up long before it got there.
    let mut rounds = 0;
    'walk: loop {
        // Ask for the next batch as soon as this one's reply is fully in
        // hand, and unpack it afterwards, so the peer is working on the next
        // request while this side is still storing the last one.
        //
        // The moment matters. A request sent while the peer is part-way
        // through writing a reply can deadlock: the peer is blocked writing
        // because this side is not reading, and this side is blocked writing
        // because the peer is not reading. Sending it only once the reply is
        // complete rules that out -- a peer that has just finished a reply has
        // gone back to reading, by construction.
        //
        // There is reliably something to ask for. Taking a batch leaves about
        // a third of the queue behind on a history of this size, because the
        // frontier is wider than one request.
        let mut batch = take_batch(&mut queue);
        if !batch.is_empty() {
            send(ws, &Msg::Want { hashes: batch.clone() })?;
            stats.round_trips += 1;
        }

        while !batch.is_empty() {
            if requested.len() > MAX_OBJECTS_PER_SYNC {
                bail!("sync exceeded {MAX_OBJECTS_PER_SYNC} objects — refusing to continue");
            }

            // The whole reply, read before the store is touched: one `Want` is
            // answered by as many messages as the peer needs to stay under its
            // size budget, ending with an empty one. Objects it holds as a
            // patch arrive as patches, after the literals they are diffed
            // against, and that order is preserved here because applying a
            // patch needs its base already in place.
            let mut reply: Vec<Incoming> = Vec::new();
            let mut held = 0usize;
            loop {
                let began = std::time::Instant::now();
                let message = recv(ws)?;
                timing::add(&timing::RECV, began);
                match message {
                    Msg::Objects { objects } if objects.is_empty() => break,
                    Msg::Objects { objects } => {
                        for (claimed, framed) in objects {
                            // Checked here rather than at the far end of the
                            // reply. Holding a peer's bytes before judging them
                            // is one thing; carrying on reading from a peer
                            // that has already been caught inventing them is
                            // another, and it is also how a lie that arrived
                            // just before the peer hung up got reported as the
                            // hanging up rather than as the lie.
                            let actual = Hash(*blake3::hash(&framed).as_bytes());
                            if actual != claimed {
                                bail!(
                                    "hash mismatch: content hashes to {} but was offered as {}",
                                    actual.short(),
                                    claimed.short()
                                );
                            }
                            held += framed.len();
                            reply.push(Incoming::Whole(claimed, framed));
                        }
                    }
                    Msg::Patches { items } => {
                        for (claimed, raw, payload) in items {
                            held += payload.len();
                            reply.push(Incoming::Patched(claimed, raw, payload));
                        }
                    }
                    other => {
                        bail!("expected an Objects or Patches message, got {}", other.name())
                    }
                }
                // A reply is one batch's worth of objects and the literals
                // their patches are diffed against. Something far past that is
                // a peer that has decided not to stop, and waiting for its
                // terminator would mean holding all of it.
                if held > REPLY_LIMIT {
                    bail!("the peer sent more than {REPLY_LIMIT} bytes in answer to one request");
                }
            }

            // The peer is reading again, so this cannot block against a peer
            // that is blocked writing.
            //
            // Whether it worked is not asked yet. This request is speculative
            // and the reply already in hand is not: a peer that has just sent
            // something wrong and hung up should be reported for the wrong
            // thing it sent, not for the write that failed afterwards.
            let next = take_batch(&mut queue);
            let asked = if next.is_empty() {
                Ok(())
            } else {
                stats.round_trips += 1;
                send(ws, &Msg::Want { hashes: next.clone() })
            };

            // Only now unpack, which is what fills the queue for the round
            // after this one. Nothing is trusted on the way in: both `put_raw`
            // and `put_patch` recompute the hash and refuse anything that does
            // not come out as the name it was offered under, so a forged
            // object cannot be stored and a forged patch cannot be applied.
            let wanted: HashSet<Hash> = batch.iter().copied().collect();
            let mut delivered = HashSet::new();
            let mut arrived: Vec<(Hash, Vec<u8>)> = Vec::with_capacity(reply.len());
            for item in reply {
                match item {
                    Incoming::Whole(claimed, framed) => {
                        if store.has(claimed) {
                            delivered.insert(claimed);
                            requested.insert(claimed);
                            continue;
                        }
                        let began = std::time::Instant::now();
                        let (id, _) = store.put_raw(claimed, &framed)?;
                        timing::add(&timing::PUT, began);
                        stats.objects += 1;
                        stats.bytes += framed.len() as u64;
                        arrived.push((id, framed));
                    }
                    Incoming::Patched(claimed, raw, payload) => {
                        if store.has(claimed) {
                            delivered.insert(claimed);
                            requested.insert(claimed);
                            continue;
                        }
                        // Kept as a patch rather than expanded and rewritten:
                        // the peer already did the work of finding the base,
                        // and undoing it here is how a 1.4 GiB history became
                        // 2.8 GiB on the far side.
                        let began = std::time::Instant::now();
                        let framed =
                            store.put_patch(claimed, raw, &payload).with_context(|| {
                                format!("applying the patch the peer sent for {}", claimed.short())
                            })?;
                        timing::add(&timing::PUT, began);
                        stats.objects += 1;
                        stats.bytes += payload.len() as u64;
                        arrived.push((claimed, framed));
                    }
                }
            }

            // Everything that arrived counts as ours before any of its links
            // are read. Marking them one at a time meant a link from the first
            // object to the fifth found the fifth already stored but not yet
            // accounted for, and read as a gap that had been pruned at.
            for (id, _) in &arrived {
                delivered.insert(*id);
                requested.insert(*id);
            }

            for (id, framed) in arrived {
                // The edges are read out of the bytes in hand. Asking the
                // store for them instead meant writing the object, flushing
                // it, and then reopening its segment to read, decompress and
                // re-verify what was already in memory -- once per object,
                // which over a large history is most of the time a push takes.
                let began = std::time::Instant::now();
                let links = Store::decode_framed(&framed)
                    .with_context(|| format!("object {} will not decode", id.short()))?
                    .links();
                timing::add(&timing::LINKS, began);
                let began = std::time::Instant::now();
                for link in links {
                    if store.has(link) {
                        // Already here, and not because this connection put it
                        // here: that is where a gap could be hiding.
                        if !requested.contains(&link) {
                            pruned = true;
                            timing::PRUNES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else if requested.insert(link) {
                        queue.push_back(link);
                    }
                }
                timing::add(&timing::HAS, began);
            }
            progress(&stats);

            if let Some(missing) = wanted.iter().find(|h| !delivered.contains(h)) {
                bail!(
                    "peer could not supply object {} — its repository is incomplete",
                    missing.short()
                );
            }

            // What arrived has been judged; now the write may be complained
            // about.
            asked?;

            batch = next;
            // Unpacking may have revealed more work while nothing is in
            // flight, which happens whenever the queue ran dry above.
            if batch.is_empty() && !queue.is_empty() {
                batch = take_batch(&mut queue);
                send(ws, &Msg::Want { hashes: batch.clone() })?;
                stats.round_trips += 1;
            }
        }

        if !pruned {
            break 'walk;
        }

        let began = std::time::Instant::now();
        let mut seen = HashSet::new();
        let mut holes = Vec::new();
        for &r in roots {
            holes.extend(missing_in_closure(store, r, &mut seen)?);
        }
        timing::add(&timing::WALK, began);
        if holes.is_empty() {
            break 'walk;
        }

        // Guard against a peer that answers without ever making progress,
        // rather than against depth: the walk below follows links, so depth
        // costs passes through the queue, not rounds.
        rounds += 1;
        if rounds > MAX_REPAIR_ROUNDS {
            bail!(
                "still incomplete after {rounds} attempts to fill the gaps; \
                 {} object(s) missing, the first being {}",
                holes.len(),
                holes[0].short()
            );
        }

        for h in holes {
            // Through the same guard as everything else: a hole that goes in
            // unguarded can be queued a second time when some other object
            // turns out to link to it, and the peer is then asked for one
            // hash twice in a single exchange.
            if requested.insert(h) {
                queue.push_back(h);
            }
        }
    }

    send(ws, &Msg::Done)?;
    Ok(stats)
}

/// **Sending half.** Answer `Want` messages until the peer says `Done`.
pub fn serve_wants<T: Transport + ?Sized>(store: &Store, ws: &mut T) -> Result<TransferStats> {
    serve_wants_watched(store, ws, &mut |_| {})
}

/// As [`serve_wants`], reporting after every reply.
///
/// A push of a large history is this side answering for minutes. Saying
/// nothing while it does is indistinguishable from having hung, which is
/// exactly how a push that had genuinely hung looked.
pub fn serve_wants_watched<T: Transport + ?Sized>(
    store: &Store,
    ws: &mut T,
    progress: &mut dyn FnMut(&TransferStats),
) -> Result<TransferStats> {
    let mut stats = TransferStats::default();
    // Bases already sent on this connection. Only ever consulted to decide
    // whether a patch's base still needs sending -- never to decide whether an
    // object that was asked for gets answered, which is always yes.
    let mut sent_bases: HashSet<Hash> = HashSet::new();
    loop {
        match recv(ws)? {
            Msg::Want { hashes } => {
                stats.round_trips += 1;

                // Objects the store already holds as a patch go over as that
                // patch, with the literal they are diffed against sent beside
                // them. Expanding them here only for the far end to store them
                // whole again threw away the compression twice: once on the
                // wire, once on the receiver's disk.
                //
                // Literals are sent before patches, so a base is always in
                // place by the time the patch that names it arrives. Depth is
                // one by construction -- a patch's base is never itself a
                // patch -- so this ordering is all the dependency there is.
                let mut literals: Vec<(Hash, Vec<u8>)> = Vec::new();
                let mut patches: Vec<(Hash, u32, Vec<u8>)> = Vec::new();
                let mut carried: HashSet<Hash> = HashSet::new();

                // Every hash asked for is answered, including one asked for
                // twice. Skipping a repeat to save the bytes meant the reply
                // quietly came back short, and a peer that is missing
                // something it was promised has no way to tell that from a
                // peer that lied -- so it gave up, on a connection that was
                // working, over an object that was right here.
                for h in hashes {
                    match store.stored_patch(h) {
                        Some((base, raw, payload)) => {
                            if sent_bases.insert(base) && carried.insert(base) {
                                let framed = store.get_raw(base).with_context(|| {
                                    format!("the base {} of a patch is gone", base.short())
                                })?;
                                stats.objects += 1;
                                stats.bytes += framed.len() as u64;
                                literals.push((base, framed));
                            }
                            stats.objects += 1;
                            stats.bytes += payload.len() as u64;
                            patches.push((h, raw, payload));
                        }
                        None => {
                            let framed = store.get_raw(h).with_context(|| {
                                format!("peer wants {} which we do not have", h.short())
                            })?;
                            stats.objects += 1;
                            stats.bytes += framed.len() as u64;
                            if carried.insert(h) {
                                literals.push((h, framed));
                            }
                        }
                    }
                }

                // A batch is a count of objects, and a count of objects is not
                // a size in bytes: one file node for a large directory can
                // outweigh a thousand chunks. Packing a whole batch into one
                // message therefore made the message unbounded, and a receiver
                // with a frame limit hangs up on it -- silently, at the TCP
                // level, so both ends saw only "the other one disappeared".
                //
                // So a reply is filled to a size and then sent, as many times
                // as it takes, and closed with an empty message. Something
                // larger than the budget still goes out on its own: the point
                // is to bound the common case, not to refuse the big one.
                let mut pending = 0usize;
                let mut batch: Vec<(Hash, Vec<u8>)> = Vec::new();
                for (h, framed) in literals {
                    let entry = framed.len() + 32 + 4;
                    if pending + entry > REPLY_BUDGET && !batch.is_empty() {
                        send(ws, &Msg::Objects { objects: std::mem::take(&mut batch) })?;
                        pending = 0;
                        progress(&stats);
                    }
                    pending += entry;
                    batch.push((h, framed));
                }
                if !batch.is_empty() {
                    send(ws, &Msg::Objects { objects: batch })?;
                }

                let mut pending = 0usize;
                let mut batch: Vec<(Hash, u32, Vec<u8>)> = Vec::new();
                for (h, raw, payload) in patches {
                    let entry = payload.len() + 32 + 4 + 4;
                    if pending + entry > REPLY_BUDGET && !batch.is_empty() {
                        send(ws, &Msg::Patches { items: std::mem::take(&mut batch) })?;
                        pending = 0;
                        progress(&stats);
                    }
                    pending += entry;
                    batch.push((h, raw, payload));
                }
                if !batch.is_empty() {
                    send(ws, &Msg::Patches { items: batch })?;
                }

                // The terminator. Waiting on "have I got everything I asked
                // for" instead would wait forever whenever the peer skipped
                // something it had already sent.
                send(ws, &Msg::Objects { objects: Vec::new() })?;
                progress(&stats);
            }
            Msg::Done => return Ok(stats),
            other => bail!("expected Want or Done, got {}", other.name()),
        }
    }
}

/// Is `ancestor` reachable from `descendant` by following parent edges?
///
/// Used to enforce fast-forward-only ref updates, so a push can never silently
/// discard commits that the server already had.
pub fn is_ancestor(store: &Store, ancestor: Hash, descendant: Hash) -> Result<bool> {
    let mut seen = HashSet::new();
    let mut stack = vec![descendant];
    while let Some(h) = stack.pop() {
        if h == ancestor {
            return Ok(true);
        }
        if !seen.insert(h) || !store.has(h) {
            continue;
        }
        if let crate::object::Object::Commit(c) = store.get(h)? {
            stack.extend(c.parents);
        }
    }
    Ok(false)
}

/// Confirm every object reachable from `root` is present locally. Run after a
/// push before moving a ref, so a ref can never point into a partial DAG.
pub fn verify_closure(store: &Store, root: Hash) -> Result<usize> {
    let mut seen = HashSet::new();
    verify_closure_into(store, root, &mut seen)
}

/// As [`verify_closure`], but remembering what has already been proven.
///
/// A push moves one ref at a time and verifies each before letting it move, so
/// a repository with a thousand tags verified the same history a thousand
/// times -- every walk from scratch, reading and materializing every object
/// along the way. Pushing git's own tags that way took about twenty seconds
/// each and pinned a core for the better part of an hour.
///
/// Carrying the set across those calls makes the cost of the whole push the
/// size of the history rather than the size of the history times the number of
/// refs. It is sound because presence is monotone within a session: a store
/// only gains objects while a push is running, so an object proven present
/// stays present, and stopping at one that has already been proven skips a
/// subtree that has already been checked.
///
/// The caller owns the set, so it is only ever shared where that is true --
/// within one connection, never across them.
pub fn verify_closure_into(
    store: &Store,
    root: Hash,
    seen: &mut HashSet<Hash>,
) -> Result<usize> {
    let before = seen.len();
    let missing = missing_in_closure(store, root, seen)?;
    if let Some(h) = missing.first() {
        bail!("incomplete: object {} is missing", h.short());
    }
    Ok(seen.len() - before)
}

/// Walk the closure under `root` and report what is not here.
///
/// The same walk as [`verify_closure_into`], but it names everything absent
/// instead of stopping at the first one, so a caller able to go and get them
/// can do that in one round rather than one at a time.
///
/// A missing object stops the descent there: its children cannot be read to be
/// asked about, and fetching it will reveal them.
pub fn missing_in_closure(
    store: &Store,
    root: Hash,
    seen: &mut HashSet<Hash>,
) -> Result<Vec<Hash>> {
    let mut missing = Vec::new();
    let mut stack = vec![root];
    while let Some(h) = stack.pop() {
        if !seen.insert(h) {
            continue;
        }
        if !store.has(h) {
            // Not proven after all -- forget it, so a later walk looks again.
            seen.remove(&h);
            missing.push(h);
            continue;
        }
        stack.extend(store.get(h)?.links());
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let cases = vec![
            Msg::Hello { repo: "myrepo".into(), token: "s3cret".into() },
            Msg::Welcome { refs: vec![("main".into(), Hash([1; 32]))] },
            Msg::PushRef { branch: "main".into(), tip: Hash([2; 32]), force: true },
            Msg::PushStash { tip: Hash([7; 32]), message: "wip".into() },
            Msg::ListStashes,
            Msg::StashList { entries: vec![(Hash([8; 32]), "half a parser".into())] },
            Msg::PullStash { commit: Hash([9; 32]) },
            Msg::DropStash { commit: Hash([10; 32]) },
            Msg::PullRef { branch: "dev".into() },
            Msg::RefIs { branch: "dev".into(), tip: None },
            Msg::RefIs { branch: "dev".into(), tip: Some(Hash([3; 32])) },
            Msg::Want { hashes: vec![Hash([4; 32]), Hash([5; 32])] },
            Msg::Objects { objects: vec![(Hash([6; 32]), vec![1, 2, 3])] },
            Msg::Patches { items: vec![(Hash([11; 32]), 4096, vec![4, 5, 6])] },
            Msg::Done,
            Msg::Ok { message: "pushed".into() },
            Msg::Error { message: "nope".into() },
        ];
        for m in cases {
            let back = Msg::decode(&m.encode()).unwrap();
            assert_eq!(back, m, "round trip failed for {m:?}");
        }
    }

    /// A transport that answers from a script and keeps what was written to it,
    /// so one side of the exchange can be examined without a second thread.
    struct Scripted {
        inbox: VecDeque<Vec<u8>>,
        wrote: Vec<Vec<u8>>,
    }

    impl Transport for Scripted {
        fn send_bytes(&mut self, payload: &[u8]) -> Result<()> {
            self.wrote.push(payload.to_vec());
            Ok(())
        }
        fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.inbox.pop_front())
        }
    }

    /// A reply is bounded by bytes, not by how many objects were asked for.
    ///
    /// `BATCH` counts objects, and objects are not a fixed size, so a reply
    /// that packed a whole batch into one message was unbounded. Nothing
    /// reported that: a websocket peer refusing an over-sized frame just
    /// closes, and both ends saw only that the other had gone away. A push of
    /// a large history therefore died instantly, on a healthy connection, with
    /// "connection reset by peer" and nothing else to go on.
    #[test]
    fn a_reply_is_bounded_by_bytes_not_by_object_count() {
        let dir = std::env::temp_dir().join(format!(
            "fkit-reply-budget-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = crate::store::Store::open(&dir).unwrap();

        // Comfortably more than one reply's worth, and incompressible enough
        // that it stays that way once stored.
        let mut payload = vec![0u8; REPLY_BUDGET * 3];
        let mut x: u32 = 0x1234_5678;
        for b in payload.iter_mut() {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (x >> 24) as u8;
        }
        let sink = crate::store::Sink::writing(&store);
        let file = crate::ingest::ingest_bytes(&sink, &payload).unwrap();

        // Everything under that file, which is what one `Want` would carry.
        let mut hashes = Vec::new();
        let mut stack = vec![file.hash];
        let mut seen = HashSet::new();
        while let Some(h) = stack.pop() {
            if !seen.insert(h) {
                continue;
            }
            hashes.push(h);
            stack.extend(store.get(h).unwrap().links());
        }
        assert!(hashes.len() <= BATCH, "one batch should cover this file");

        let mut t = Scripted {
            inbox: VecDeque::from(vec![
                Msg::Want { hashes: hashes.clone() }.encode(),
                Msg::Done.encode(),
            ]),
            wrote: Vec::new(),
        };
        serve_wants(&store, &mut t).unwrap();

        let replies: Vec<Msg> = t.wrote.iter().map(|b| Msg::decode(b).unwrap()).collect();
        let biggest = t.wrote.iter().map(|b| b.len()).max().unwrap();
        let largest_object = hashes
            .iter()
            .map(|h| store.get_raw(*h).unwrap().len())
            .max()
            .unwrap();

        // One object always goes out even if it alone is over budget, so the
        // ceiling is the budget plus the largest single object -- not the
        // sum of a batch, which is what it used to be.
        assert!(
            biggest <= REPLY_BUDGET + largest_object + 1024,
            "a reply message reached {biggest} bytes, past the {REPLY_BUDGET} byte budget"
        );
        assert!(
            replies.len() > 2,
            "this payload should have needed splitting, got {} message(s)",
            replies.len()
        );
        assert!(
            matches!(replies.last(), Some(Msg::Objects { objects }) if objects.is_empty()),
            "a reply must end with an empty message, or the reader cannot tell where it stops"
        );

        // And the split loses nothing.
        let mut got = HashSet::new();
        for r in &replies {
            let Msg::Objects { objects } = r else { panic!("not an Objects message") };
            for (h, _) in objects {
                got.insert(*h);
            }
        }
        assert_eq!(got, hashes.into_iter().collect::<HashSet<_>>());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
