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
pub const BATCH: usize = 256;

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
            0x22 => Msg::Done,
            0x30 => Msg::Ok { message: r.str()? },
            0x31 => Msg::Error { message: r.str()? },
            other => bail!("unknown protocol message type 0x{other:02X}"),
        })
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
    let mut stats = TransferStats::default();
    let mut queue: VecDeque<Hash> = VecDeque::new();
    let mut requested: HashSet<Hash> = HashSet::new();

    for &r in roots {
        if !store.has(r) && requested.insert(r) {
            queue.push_back(r);
        }
    }

    while !queue.is_empty() {
        if requested.len() > MAX_OBJECTS_PER_SYNC {
            bail!("sync exceeded {MAX_OBJECTS_PER_SYNC} objects — refusing to continue");
        }

        let batch: Vec<Hash> = (0..BATCH).filter_map(|_| queue.pop_front()).collect();
        send(ws, &Msg::Want { hashes: batch.clone() })?;
        stats.round_trips += 1;

        let Msg::Objects { objects } = recv(ws)? else {
            bail!("expected an Objects message");
        };

        let wanted: HashSet<Hash> = batch.iter().copied().collect();
        let mut delivered = HashSet::new();

        for (claimed, framed) in objects {
            if !wanted.contains(&claimed) {
                bail!("peer sent object {} that we never asked for", claimed.short());
            }
            // put_raw recomputes the hash; a lying peer is rejected here.
            let (id, _) = store.put_raw(claimed, &framed)?;
            delivered.insert(id);
            stats.objects += 1;
            stats.bytes += framed.len() as u64;

            // Follow the newly-revealed edges.
            for link in store.get(id)?.links() {
                if !store.has(link) && requested.insert(link) {
                    queue.push_back(link);
                }
            }
        }

        if let Some(missing) = wanted.iter().find(|h| !delivered.contains(h)) {
            bail!(
                "peer could not supply object {} — its repository is incomplete",
                missing.short()
            );
        }
    }

    send(ws, &Msg::Done)?;
    Ok(stats)
}

/// **Sending half.** Answer `Want` messages until the peer says `Done`.
pub fn serve_wants<T: Transport + ?Sized>(store: &Store, ws: &mut T) -> Result<TransferStats> {
    let mut stats = TransferStats::default();
    loop {
        match recv(ws)? {
            Msg::Want { hashes } => {
                stats.round_trips += 1;
                let mut objects = Vec::with_capacity(hashes.len());
                for h in hashes {
                    let framed = store
                        .get_raw(h)
                        .with_context(|| format!("peer wants {} which we do not have", h.short()))?;
                    stats.objects += 1;
                    stats.bytes += framed.len() as u64;
                    objects.push((h, framed));
                }
                send(ws, &Msg::Objects { objects })?;
            }
            Msg::Done => return Ok(stats),
            other => bail!("expected Want or Done, got {other:?}"),
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
    let mut stack = vec![root];
    while let Some(h) = stack.pop() {
        if !seen.insert(h) {
            continue;
        }
        if !store.has(h) {
            bail!("incomplete: object {} is missing", h.short());
        }
        stack.extend(store.get(h)?.links());
    }
    Ok(seen.len() - before)
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
            Msg::Done,
            Msg::Ok { message: "pushed".into() },
            Msg::Error { message: "nope".into() },
        ];
        for m in cases {
            let back = Msg::decode(&m.encode()).unwrap();
            assert_eq!(back, m, "round trip failed for {m:?}");
        }
    }
}
