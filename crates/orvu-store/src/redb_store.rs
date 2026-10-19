//! The standard store: one `redb` database file. A table maps a number that
//! goes up by one for each delta to that delta's bytes, so `replay` returns
//! deltas in exactly the order they were appended. An appended delta is held
//! in memory at first and is only safely on disk after `flush`. A `redb`
//! commit waits for the disk to confirm the write unless told otherwise. How
//! often to flush is up to the caller.

use crate::{peer_lacks, Receipt, Result, Store, StoreError};
use orvu_core::{
    decode_snapshot_entry, decode_wire_delta, encode_snapshot_entry, encode_wire_delta,
    SnapshotEntry, WireDelta,
};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::path::Path;
use orvu_crdt::DotContext;
use std::collections::BTreeMap;

const DELTAS: TableDefinition<u64, &[u8]> = TableDefinition::new("deltas");
const SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("snapshots");
/// The receipts, by when they were made and then by id, so that the old ones
/// can be let go from the front.
const RECEIPTS: TableDefinition<(u64, [u8; 16]), ()> = TableDefinition::new("receipts");

/// The most deltas held in memory waiting for a flush. An append that would
/// go past it flushes first, so a slow flush timer or a big batch of changes
/// never holds more than this in memory. Only if that flush fails is the
/// append refused.
pub const UNSAVED_LIMIT: usize = 100_000;

pub struct RedbStore {
    db: Database,
    next_seq: u64,
    pending: Vec<(u64, Vec<u8>)>,
    /// Receipts recorded since the last flush. They are saved in the same
    /// step as the pending deltas.
    pending_receipts: Vec<Receipt>,
    pub(crate) unsaved_limit: usize,
    /// Makes `flush` fail, so tests can see what happens when saving fails.
    #[cfg(test)]
    pub(crate) fail_flush: bool,
}

/// Turns any `redb` error into the matching `StoreError` kind.
fn backend<E: Into<redb::Error>>(e: E) -> StoreError {
    StoreError::from(e.into())
}

impl From<redb::Error> for StoreError {
    fn from(e: redb::Error) -> Self {
        match e {
            redb::Error::DatabaseAlreadyOpen => StoreError::AlreadyOpen,
            // redb reports a file that is not a database at all, or whose
            // header does not make sense, as "invalid data".
            redb::Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidData => {
                StoreError::Corrupted(io.to_string())
            }
            redb::Error::Io(io) => StoreError::Io(io),
            // An earlier write failed, and redb will not write again until
            // the file is reopened. It is still a failure to write.
            redb::Error::PreviousIo => StoreError::Io(std::io::Error::other(
                "an earlier write to the store file failed",
            )),
            redb::Error::Corrupted(what) => StoreError::Corrupted(what),
            other => StoreError::Other(other.to_string()),
        }
    }
}

impl RedbStore {
    /// Opens the delta log stored at `path`, creating it if it is not there.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path).map_err(backend)?;
        // Create the table if it is missing, and work out the next number.
        let next_seq = {
            let wtx = db.begin_write().map_err(backend)?;
            let next = {
                let table = wtx.open_table(DELTAS).map_err(backend)?;
                // Put the last row in a variable first. Matching on
                // `table.last()?` directly would keep the row borrowed for
                // longer than `table` lives, and that does not compile.
                let last = table.last().map_err(backend)?;
                match last {
                    Some((k, _)) => k.value() + 1,
                    None => 0,
                }
            };
            wtx.open_table(SNAPSHOTS).map_err(backend)?; // make sure it exists, so reads can open it
            wtx.open_table(RECEIPTS).map_err(backend)?;
            wtx.commit().map_err(backend)?; // save the new tables
            next
        };
        Ok(Self {
            db,
            next_seq,
            pending: Vec::new(),
            pending_receipts: Vec::new(),
            unsaved_limit: UNSAVED_LIMIT,
            #[cfg(test)]
            fail_flush: false,
        })
    }
}

impl Store for RedbStore {
    fn append(&mut self, delta: &WireDelta) -> Result<()> {
        // Too many waiting: save them before taking more. Only if that save
        // fails is the append refused, so memory stays bounded either way.
        if self.pending.len() >= self.unsaved_limit {
            self.flush()?;
        }
        self.pending.push((self.next_seq, encode_wire_delta(delta)));
        self.next_seq += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        #[cfg(test)]
        if self.fail_flush {
            return Err(StoreError::Io(std::io::ErrorKind::StorageFull.into()));
        }
        if self.pending.is_empty() && self.pending_receipts.is_empty() {
            return Ok(());
        }
        let wtx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = wtx.open_table(DELTAS).map_err(backend)?;
            for (seq, bytes) in &self.pending {
                table.insert(*seq, bytes.as_slice()).map_err(backend)?;
            }
        }
        // The receipts go in the same step as the deltas they follow.
        if let Some(newest) = self.pending_receipts.iter().map(|receipt| receipt.at_ms).max() {
            let mut table = wtx.open_table(RECEIPTS).map_err(backend)?;
            for receipt in &self.pending_receipts {
                table.insert((receipt.at_ms, receipt.id), ()).map_err(backend)?;
            }
            let cutoff = newest.saturating_sub(crate::RECEIPT_KEEP_MS);
            table.retain_in(..(cutoff, [0u8; 16]), |_, _| false).map_err(backend)?;
        }
        wtx.commit().map_err(backend)?; // by default this waits until the data is on disk
        self.pending.clear();
        self.pending_receipts.clear();
        Ok(())
    }

    fn receipt(&mut self, receipt: Receipt) -> Result<()> {
        self.pending_receipts.push(receipt);
        Ok(())
    }

    fn receipts(&self) -> Result<Vec<Receipt>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(RECEIPTS).map_err(backend)?;
        let mut out = Vec::new();
        for row in table.iter().map_err(backend)? {
            let (key, _) = row.map_err(backend)?;
            let (at_ms, id) = key.value();
            out.push(Receipt { id, at_ms });
        }
        out.extend(self.pending_receipts.iter().copied());
        out.sort_by_key(|receipt| (receipt.at_ms, receipt.id));
        Ok(out)
    }

    fn replay(&self) -> Result<Vec<WireDelta>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(DELTAS).map_err(backend)?;
        let mut out = Vec::with_capacity(table.len().map_err(backend)? as usize + self.pending.len());
        for row in table.iter().map_err(backend)? {
            let (_seq, bytes) = row.map_err(backend)?;
            out.push(decode_wire_delta(bytes.value())?);
        }
        // Deltas appended but not flushed yet come last. Their numbers are
        // higher than the saved ones, so this keeps the order they were
        // appended in.
        for (_seq, bytes) in &self.pending {
            out.push(decode_wire_delta(bytes)?);
        }
        Ok(out)
    }

    fn compact_to(&mut self, snapshot: &[SnapshotEntry], tail: &[WireDelta]) -> Result<()> {
        let wtx = self.db.begin_write().map_err(backend)?;
        wtx.delete_table(SNAPSHOTS).map_err(backend)?;
        wtx.delete_table(DELTAS).map_err(backend)?;
        {
            let mut snaps = wtx.open_table(SNAPSHOTS).map_err(backend)?;
            for e in snapshot {
                snaps
                    .insert(e.object_id.as_str(), encode_snapshot_entry(e).as_slice())
                    .map_err(backend)?;
            }
        }
        {
            let mut deltas = wtx.open_table(DELTAS).map_err(backend)?;
            for (i, d) in tail.iter().enumerate() {
                deltas
                    .insert(i as u64, encode_wire_delta(d).as_slice())
                    .map_err(backend)?;
            }
        }
        // Receipts are not part of the log, and stay. Those not saved yet
        // are saved now, as the deltas they follow are.
        {
            let mut receipts = wtx.open_table(RECEIPTS).map_err(backend)?;
            for receipt in &self.pending_receipts {
                receipts.insert((receipt.at_ms, receipt.id), ()).map_err(backend)?;
            }
        }
        wtx.commit().map_err(backend)?;
        self.next_seq = tail.len() as u64;
        self.pending.clear();
        self.pending_receipts.clear();
        Ok(())
    }

    fn load_snapshot(&self) -> Result<Vec<SnapshotEntry>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(SNAPSHOTS).map_err(backend)?;
        let mut out = Vec::with_capacity(table.len().map_err(backend)? as usize);
        for row in table.iter().map_err(backend)? {
            let (_id, bytes) = row.map_err(backend)?;
            out.push(decode_snapshot_entry(bytes.value())?);
        }
        Ok(out)
    }

    fn deltas_peer_lacks(&self, peer: &BTreeMap<String, DotContext>) -> Result<Vec<WireDelta>> {
        let lacks = |d: &WireDelta| peer_lacks(peer, d);
        let mut out = Vec::new();
        // Deltas already saved to disk.
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(DELTAS).map_err(backend)?;
        for row in table.iter().map_err(backend)? {
            let (_seq, bytes) = row.map_err(backend)?;
            let d = decode_wire_delta(bytes.value())?;
            if lacks(&d) {
                out.push(d);
            }
        }
        // Then the deltas appended but not flushed yet, as in `replay`.
        for (_seq, bytes) in &self.pending {
            let d = decode_wire_delta(bytes)?;
            if lacks(&d) {
                out.push(d);
            }
        }
        Ok(out)
    }

    fn snapshot_contexts(&self) -> Result<BTreeMap<String, DotContext>> {
        Ok(self
            .load_snapshot()?
            .into_iter()
            .map(|e| (e.object_id, e.context))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::delta; // the helper from the tests in lib.rs

    // A second open of a file that is already open is told apart from other
    // failures, so a caller can say "another program is using it".
    #[test]
    fn opening_an_open_file_again_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let _first = RedbStore::open(&path).unwrap();
        match RedbStore::open(&path) {
            Err(StoreError::AlreadyOpen) => {}
            Err(other) => panic!("expected AlreadyOpen, got {other:?}"),
            Ok(_) => panic!("the second open should fail"),
        }
    }

    // A file full of junk is a damaged store, not a passing glitch.
    #[test]
    fn a_damaged_file_is_reported_as_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        std::fs::write(&path, vec![0xAB; 64 * 1024]).unwrap();
        match RedbStore::open(&path) {
            Err(StoreError::Corrupted(_)) => {}
            Err(other) => panic!("expected Corrupted, got {other:?}"),
            Ok(_) => panic!("opening junk should fail"),
        }
    }

    // A disk that is full comes through as an I/O error that keeps its kind.
    #[test]
    fn a_full_disk_keeps_its_io_kind() {
        let full = redb::Error::Io(std::io::Error::from(std::io::ErrorKind::StorageFull));
        match StoreError::from(full) {
            StoreError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::StorageFull),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    // Deltas waiting to be flushed are held in memory. When too many are
    // waiting, the store saves them before taking more, so a slow timer or a
    // big batch of changes does not fail it. Nothing is refused while saving
    // works.
    #[test]
    fn appending_past_the_unsaved_limit_saves_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.unsaved_limit = 3;
            for c in 1..=10 {
                s.append(&delta(c)).unwrap();
            }
            assert!(s.pending.len() <= 3, "more than the limit is held in memory");
            let got: Vec<u64> = s.replay().unwrap().iter().map(|d| d.dot.counter).collect();
            assert_eq!(got, (1..=10).collect::<Vec<_>>());
        } // dropped without a flush, which stands in for a crash
        let saved = RedbStore::open(&path).unwrap().replay().unwrap().len();
        assert!(saved >= 7, "the deltas past the limit were not saved first: {saved} on disk");
    }

    // If that save fails, the append fails and memory stays bounded.
    #[test]
    fn appending_past_the_unsaved_limit_fails_if_saving_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        s.unsaved_limit = 3;
        for c in 1..=3 {
            s.append(&delta(c)).unwrap();
        }
        s.fail_flush = true;
        match s.append(&delta(4)) {
            Err(StoreError::Io(_)) => {}
            other => panic!("expected the failed save, got {other:?}"),
        }
        // Nothing past the limit was kept.
        let got: Vec<u64> = s.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 2, 3]);
        // Once saving works again, appending works again.
        s.fail_flush = false;
        s.append(&delta(4)).unwrap();
    }

    // A receipt is saved in the same step as the change before it: both are
    // there after a restart, or neither is.
    #[test]
    fn a_receipt_is_saved_with_the_change_it_follows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let receipt = Receipt { id: [7; 16], at_ms: 5_000 };
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap();
            s.receipt(receipt).unwrap();
            assert_eq!(s.receipts().unwrap(), vec![receipt], "an unsaved receipt is seen, as an unsaved delta is");
            // Dropped without a flush, which stands in for a crash.
        }
        {
            let mut s = RedbStore::open(&path).unwrap();
            assert!(s.replay().unwrap().is_empty() && s.receipts().unwrap().is_empty());
            s.append(&delta(1)).unwrap();
            s.receipt(receipt).unwrap();
            s.flush().unwrap();
        }
        let mut s = RedbStore::open(&path).unwrap();
        assert_eq!(s.replay().unwrap().len(), 1);
        assert_eq!(s.receipts().unwrap(), vec![receipt]);
        // Compacting the log keeps the receipts.
        s.compact(&[]).unwrap();
        assert_eq!(s.receipts().unwrap(), vec![receipt]);
    }

    // A receipt older than the time they are kept for is let go when a newer
    // one is saved.
    #[test]
    fn old_receipts_are_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        let old = Receipt { id: [1; 16], at_ms: 1_000 };
        let kept = Receipt { id: [2; 16], at_ms: 2_000 };
        let new = Receipt { id: [3; 16], at_ms: 1_000 + crate::RECEIPT_KEEP_MS + 1 };
        for receipt in [old, kept] {
            s.receipt(receipt).unwrap();
            s.flush().unwrap();
        }
        assert_eq!(s.receipts().unwrap(), vec![old, kept]);
        s.receipt(new).unwrap();
        s.flush().unwrap();
        assert_eq!(s.receipts().unwrap(), vec![kept, new]);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap();
            s.append(&delta(2)).unwrap();
            s.flush().unwrap();
        }
        let s2 = RedbStore::open(&path).unwrap();
        let got: Vec<u64> = s2.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn unflushed_appends_are_not_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap(); // dropped without a flush, which stands in for a crash
        }
        let s2 = RedbStore::open(&path).unwrap();
        assert!(s2.replay().unwrap().is_empty(), "unflushed append lost after crash");
    }

    #[test]
    fn replay_includes_unflushed_appends() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        s.append(&delta(1)).unwrap();
        s.flush().unwrap();
        s.append(&delta(2)).unwrap();
        let got: Vec<u64> = s.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 2]);
    }

    // Compacting replaces the whole log, unflushed deltas included. A snapshot
    // built from `replay` must therefore hold those deltas too.
    #[test]
    fn compacting_from_replay_keeps_unflushed_deltas() {
        use orvu_crdt::{Dot, ReplicaId};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap();
            s.flush().unwrap();
            s.append(&delta(2)).unwrap();
            let snapshot = orvu_core::fold_into_snapshot(s.load_snapshot().unwrap(), s.replay().unwrap());
            s.compact(&snapshot).unwrap();
        }
        let s2 = RedbStore::open(&path).unwrap();
        let ctx = &s2.snapshot_contexts().unwrap()["o"];
        assert!(ctx.contains(Dot::new(ReplicaId(1), 1)));
        assert!(ctx.contains(Dot::new(ReplicaId(1), 2)), "the unflushed delta is in the snapshot");
    }

    #[test]
    fn snapshot_is_kept_by_object_id() {
        use orvu_core::{SnapshotEntry, TypedCrdt};
        use orvu_crdt::{Dot, DotContext, ReplicaId};
        let entry = |id: &str, counter: u64| {
            let mut ctx = DotContext::new();
            ctx.insert(Dot::new(ReplicaId(1), counter));
            SnapshotEntry {
                object_id: id.to_string(),
                state: TypedCrdt::PnCounter(Default::default()).snapshot(),
                context: ctx,
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let mut s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        s.compact(&[entry("b", 1), entry("a", 1), entry("b", 2)]).unwrap();
        assert_eq!(s.load_snapshot().unwrap(), vec![entry("a", 1), entry("b", 2)]);
    }

    #[test]
    fn seq_continues_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap();
            s.flush().unwrap();
        }
        let mut s2 = RedbStore::open(&path).unwrap();
        s2.append(&delta(9)).unwrap();
        s2.flush().unwrap();
        let got: Vec<u64> = s2.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 9], "reopened store appends after existing data");
    }

    #[test]
    fn compact_snapshots_and_truncates_then_survives_reopen() {
        use orvu_crdt::DotContext;
        use orvu_core::{SnapshotEntry, TypedCrdt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let entry = SnapshotEntry {
            object_id: "o".into(),
            state: TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: DotContext::new(),
        };
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap();
            s.flush().unwrap();
            s.compact(std::slice::from_ref(&entry)).unwrap();
            // Deltas appended after compaction go into the new, empty log.
            s.append(&delta(2)).unwrap();
            s.flush().unwrap();
        }
        let s2 = RedbStore::open(&path).unwrap();
        assert_eq!(s2.load_snapshot().unwrap(), vec![entry], "snapshot durable");
        let tail: Vec<u64> = s2.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(tail, vec![2], "only post-compaction deltas remain in the log");
    }

    #[test]
    fn load_snapshot_is_empty_before_any_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        assert!(s.load_snapshot().unwrap().is_empty());
    }

    #[test]
    fn deltas_peer_lacks_and_snapshot_contexts_over_redb() {
        use orvu_crdt::{Dot, DotContext, ReplicaId};
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let mut s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        s.append(&delta(1)).unwrap();
        s.append(&delta(2)).unwrap();
        s.flush().unwrap();

        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        let mut peer = BTreeMap::new();
        peer.insert("o".to_string(), ctx);
        let lacked: Vec<u64> =
            s.deltas_peer_lacks(&peer).unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(lacked, vec![2]);
        assert!(s.snapshot_contexts().unwrap().is_empty(), "no compaction yet");
    }

    #[test]
    fn deltas_peer_lacks_includes_unflushed_pending() {
        use orvu_crdt::DotContext;
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().unwrap();
        let mut s = RedbStore::open(dir.path().join("t.redb")).unwrap();
        s.append(&delta(1)).unwrap();
        s.flush().unwrap(); // (1,1) is saved
        s.append(&delta(2)).unwrap(); // (1,2) stays in `pending` and is not flushed

        // A machine that has seen nothing must be offered both deltas: the
        // saved one and the one still waiting to be flushed.
        let lacked: Vec<u64> = s
            .deltas_peer_lacks(&BTreeMap::<String, DotContext>::new())
            .unwrap()
            .iter()
            .map(|d| d.dot.counter)
            .collect();
        assert_eq!(lacked, vec![1, 2], "pending (unflushed) delta is servable");
    }

    #[test]
    fn redb_compact_to_retains_the_tail_across_reopen() {
        use orvu_crdt::DotContext;
        use orvu_core::{SnapshotEntry, TypedCrdt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let entry = SnapshotEntry {
            object_id: "o".into(),
            state: TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: DotContext::new(),
        };
        {
            let mut s = RedbStore::open(&path).unwrap();
            s.append(&delta(1)).unwrap();
            s.flush().unwrap();
            s.compact_to(std::slice::from_ref(&entry), std::slice::from_ref(&delta(9))).unwrap();
        }
        let s2 = RedbStore::open(&path).unwrap();
        assert_eq!(s2.load_snapshot().unwrap(), vec![entry], "snapshot durable");
        let tail: Vec<u64> = s2.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(tail, vec![9], "retained tail durable");
    }
}
