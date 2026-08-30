//! End-to-end sync tests over a real TCP loopback and a real WebSocket
//! handshake — no mocks. These cover the layer where the hand-written bugs
//! actually lived.

use fkit_core::hash::Hash;
use fkit_core::ingest::{ingest_bytes, read_file};
use fkit_core::object::{Commit, EntryKind, Object, TreeEntry};
use fkit_core::proto::{fetch_closure, serve_wants, verify_closure};
use fkit_core::store::{Sink, Store};
use fkit_core::ws::WebSocket;
use std::net::TcpListener;
use std::path::PathBuf;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "fkit-it-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a small repository's worth of objects and return the root commit.
fn seed(store: &Store, payload: &[u8], message: &str, parent: Option<Hash>) -> Hash {
    let sink = Sink::writing(store);
    let blob = ingest_bytes(&sink, payload).unwrap();
    let readme = ingest_bytes(&sink, b"# hello\n").unwrap();

    // Entries must be sorted; build_tree asserts it and the hash depends on it.
    let (tree, _, _) = fkit_core::ingest::build_tree(
        &sink,
        vec![
            TreeEntry {
                name: "README.md".into(),
                kind: EntryKind::File { exec: false },
                hash: readme.hash,
                size: readme.size,
            },
            TreeEntry {
                name: "data.bin".into(),
                kind: EntryKind::File { exec: false },
                hash: blob.hash,
                size: blob.size,
            },
        ]
        .into_iter()
        .collect::<Vec<_>>(),
    )
    .unwrap();

    let (commit, _) = store
        .put(&Object::Commit(Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: "tester".into(),
            timestamp: 1_700_000_000,
            message: message.into(),
        }))
        .unwrap();
    commit
}

/// Run `serve_wants` on a background thread against a real socket, and
/// `fetch_closure` on this one. Returns the receiver's transfer stats.
fn transfer(sender: &Store, receiver: &Store, root: Hash) -> fkit_core::proto::TransferStats {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server_store = sender.root().to_path_buf();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (mut ws, _path) = WebSocket::accept(stream).unwrap();
        let store = Store::open(server_store).unwrap();
        serve_wants(&store, &mut ws).unwrap();
    });

    let mut ws = WebSocket::connect(&format!("ws://{addr}/repo")).unwrap();
    let stats = fetch_closure(receiver, &mut ws, &[root]).unwrap();
    ws.close();
    server.join().unwrap();
    stats
}

#[test]
fn full_closure_transfers_over_a_real_websocket() {
    let (a, b) = (Tmp::new("send"), Tmp::new("recv"));
    let sender = Store::open(&a.0).unwrap();
    let receiver = Store::open(&b.0).unwrap();

    let payload: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let commit = seed(&sender, &payload, "initial", None);

    let stats = transfer(&sender, &receiver, commit);
    assert!(stats.objects > 100, "expected a real transfer, got {stats:?}");

    // The receiver must hold the complete DAG...
    let count = verify_closure(&receiver, commit).unwrap();
    assert!(count > 100);

    // ...and be able to reconstruct the original bytes byte-for-byte.
    let Object::Commit(c) = receiver.get(commit).unwrap() else { panic!() };
    let entries = fkit_core::ingest::read_entries(&receiver, c.tree).unwrap();
    let data = entries.iter().find(|e| e.name == "data.bin").unwrap();
    let mut out = Vec::new();
    read_file(&receiver, data.hash, &mut out).unwrap();
    assert_eq!(out, payload, "reassembled payload must match the sender's");
}

/// The property that makes sync cheap: a second push of a near-identical
/// commit must transfer only the objects that actually changed.
#[test]
fn incremental_sync_transfers_only_the_delta() {
    let (a, b) = (Tmp::new("send2"), Tmp::new("recv2"));
    let sender = Store::open(&a.0).unwrap();
    let receiver = Store::open(&b.0).unwrap();

    let mut payload: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let c1 = seed(&sender, &payload, "first", None);
    let first = transfer(&sender, &receiver, c1);

    // Change one byte and commit again.
    payload[1_000_000] ^= 0xFF;
    let c2 = seed(&sender, &payload, "second", Some(c1));
    let second = transfer(&sender, &receiver, c2);

    assert!(
        second.objects < first.objects / 20,
        "incremental sync moved {} objects vs {} for the full transfer — \
         the Merkle skip is not working",
        second.objects,
        first.objects
    );
    verify_closure(&receiver, c2).unwrap();
}

/// Syncing something the receiver already has must be a no-op on the wire.
#[test]
fn syncing_an_already_present_commit_transfers_nothing() {
    let (a, b) = (Tmp::new("send3"), Tmp::new("recv3"));
    let sender = Store::open(&a.0).unwrap();
    let receiver = Store::open(&b.0).unwrap();

    let commit = seed(&sender, b"small payload", "only", None);
    transfer(&sender, &receiver, commit);
    let again = transfer(&sender, &receiver, commit);

    assert_eq!(again.objects, 0, "re-sync should transfer no objects");
    assert_eq!(again.round_trips, 0, "re-sync should need no round trips");
}

/// A peer that serves bytes not matching the requested hash must be rejected,
/// and must not be able to poison the store.
#[test]
fn a_lying_peer_is_rejected() {
    let t = Tmp::new("liar");
    let store = Store::open(&t.0).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (mut ws, _) = WebSocket::accept(stream).unwrap();
        // Answer the first Want with garbage under the requested name.
        let msg = ws.recv().unwrap().unwrap();
        let fkit_core::proto::Msg::Want { hashes } =
            fkit_core::proto::Msg::decode(&msg).unwrap()
        else {
            panic!("expected Want")
        };
        let reply = fkit_core::proto::Msg::Objects {
            objects: vec![(hashes[0], b"\x01these are not the bytes you asked for".to_vec())],
        };
        ws.send(&reply.encode()).unwrap();
    });

    let wanted = Hash([0x42; 32]);
    let mut ws = WebSocket::connect(&format!("ws://{addr}/repo")).unwrap();
    let err = fetch_closure(&store, &mut ws, &[wanted]).unwrap_err();

    assert!(
        err.to_string().contains("hash mismatch"),
        "expected a hash mismatch rejection, got: {err}"
    );
    assert!(!store.has(wanted), "the store must not be poisoned");
    let _ = server.join();
}

/// A receiver left with holes can still be filled.
///
/// This is what a dropped connection leaves behind: some objects arrived, the
/// ones beneath them did not, and the ref never moved.
///
/// The hard part is that asking for the closure prunes at everything already
/// present, so the objects above a gap are seen, skipped, and the gap with
/// them. That made a retry transfer nothing and fail on exactly the objects
/// the interruption lost, identically every time -- the only way out being to
/// delete the repository. So a transfer now looks for holes before it reports
/// itself finished, and simply running it again is the repair.
#[test]
fn a_receiver_with_holes_is_repaired_rather_than_refused() {
    let (a, b) = (Tmp::new("holesend"), Tmp::new("holerecv"));
    let sender = Store::open(&a.0).unwrap();
    let receiver = Store::open(&b.0).unwrap();

    let payload: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let commit = seed(&sender, &payload, "initial", None);

    // Copy everything except one leaf: the shape an interrupted push leaves,
    // where the objects above a gap are present and point straight at it.
    let mut hole = None;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![commit];
    while let Some(h) = stack.pop() {
        if !seen.insert(h) {
            continue;
        }
        let obj = sender.get(h).unwrap();
        let links = obj.links();
        if links.is_empty() && hole.is_none() {
            hole = Some(h);
            continue;
        }
        stack.extend(links);
        receiver.put_raw(h, &sender.get_raw(h).unwrap()).unwrap();
    }
    let hole = hole.expect("the history should contain a leaf to remove");
    assert!(!receiver.has(hole), "the hole must actually be missing");

    // The gap is real, and a walk of the closure names it.
    let mut seen = std::collections::HashSet::new();
    let missing = fkit_core::proto::missing_in_closure(&receiver, commit, &mut seen).unwrap();
    assert_eq!(missing, vec![hole], "the walk should name exactly the gap");

    // Simply asking again is enough. Pruning at what is present would skip
    // straight past this hole -- that is what used to make the failure
    // permanent -- so the transfer checks itself before reporting success.
    let stats = transfer(&sender, &receiver, commit);
    assert_eq!(stats.objects, 1, "a retry should fetch the one missing object");
    assert!(receiver.has(hole), "and the hole should be filled");

    let mut seen = std::collections::HashSet::new();
    assert!(
        fkit_core::proto::missing_in_closure(&receiver, commit, &mut seen).unwrap().is_empty(),
        "nothing should be missing once the gap is filled"
    );
    verify_closure(&receiver, commit).expect("the closure is whole again");
}
