#[allow(dead_code)]

use std::collections::Bound;
use std::iter::*;

extern crate rand;
extern crate owning_ref;

mod memstore;
use memstore::*;
mod disk;
use disk::*;
mod toc;
use toc::*;
mod error;
mod util;
use util::*;
mod iter;
use iter::*;
mod encoding;


pub struct Store {
    // Never empty
    memstores: Vec<MemStore>,
    threshold: usize,
    directory: String,
    toc_file: std::fs::File,
    toc: TOC,
}

pub struct StoreIter<'a> {
    interval: Interval<String>,
    iters: MergeIterator<'a>,
}

impl Store {
    pub fn create(dir: &str) -> Result<()> {
        // NOTE: We'll want directory locking and such.
        // NOTE: Pass errors up
        std::fs::create_dir(dir).expect("create_dir");  // NOTE: Never use expect
        create_toc(dir).expect("create_toc");
        return Ok(());
    }

    pub fn open(dir: &str, threshold: usize) -> Result<Store> {
        // NOTE: Review if we should error upon some of these error cases.
        // NOTE: Better error handling
        // NOTE: Clean up optional chaining.

        let (toc_file, toc) = read_toc(dir).expect("read_toc");
        let mut ms = MemStore::new();
        for fileno in 0..toc.next_table_id {
            if let Some(table_info) = toc.table_infos.get(&fileno) {
                iterate_table(dir, table_info, &mut |key: String, value: Mutation| {
                    ms.apply(key, value);
                })?;
            }
        }

        return Ok(Store::make_existing(threshold, dir.to_string(), toc_file, toc, ms));
    }

    pub fn make(threshold: usize, directory: String, toc_file: std::fs::File, toc: TOC) -> Store {
        return Store::make_existing(threshold, directory, toc_file, toc, MemStore::new());
    }

    fn make_existing(threshold: usize, directory: String, toc_file: std::fs::File, toc: TOC, ms: MemStore) -> Store {
        return Store{
            memstores: vec![MemStore::new(), ms],
            threshold: threshold,

            directory: directory,
            toc_file: toc_file,
            toc: toc,
        }
    }

    pub fn insert(&mut self, key: &str, val: &str) -> Result<bool> {
        if !self.exists(key) {
            self.put(key, val)?;
            return Ok(true);
        }
        return Ok(false);
    }

    pub fn replace(&mut self, key: &str, val: &str) -> Result<bool> {
        if self.exists(key) {
            self.put(key, val)?;
            return Ok(true);
        }
        return Ok(false);
    }

    pub fn put(&mut self, key: &str, val: &str) -> Result<()> {
        self.memstores[0].apply(key.to_string(), Mutation::Set(val.to_string()));
        return self.consider_split();
    }

    pub fn remove(&mut self, key: &str) -> Result<bool> {
        if self.exists(key) {
            self.memstores[0].apply(key.to_string(), Mutation::Delete);
            self.consider_split()?;
            return Ok(true);
        }
        return Ok(false);
    }

    pub fn flush(&mut self) -> Result<()> {
        let ms: MemStore = self.memstores.remove(0);

        // NOTE: Instead of flushing and compacting, we could, you know, do a
        // flush into the compaction.
        self.flush_and_record(0, &ms)?;
        self.rebalance()?;

        self.memstores.insert(0, MemStore::new());
        return Ok(());
    }

    pub fn rebalance(&mut self) -> Result<()> {
        if self.toc.level_infos.get(&0).map_or(false, |lz| lz.len() > 4) {
            // Do a releveling with all but the latest (highest numbered) table.
            let table_ids: Vec<TableId>
                = self.toc.level_infos.get(&0).unwrap().iter().rev().skip(1).map(|&x| x).collect();
            self.relevel(0, table_ids)?;
            // Exit.  Don't do more than one releveling per "rebalance"
            // operation.  Just to spread the work out, barely.
            return Ok(());
        }

        // NOTE: We might want to spread out pending necessary relevelings
        // instead of doing them all in a row.  We might need to do more than
        // one releveling at a time, in order to keep up with writes, though.
        // Basically, expect each releveling at level 0 to kick off a bunch of
        // relevelings at level 1, 2, 3, 4, ...

        // NOTE: Maybe relevel a batch of N consecutive files at once, instead
        // of just 1 at a time.  This will minimize overhead of dealing with
        // edges.  We'd probably have to relevel 4 at a time, no?

        let max_level: LevelNumber
            = self.toc.level_infos.iter().map(|(&level, _)| level).max().expect("at least one level");

        for level in 1..max_level {
            let to_relevel: (LevelNumber, TableId);
            if let Some(table_ids) = self.toc.level_infos.get(&level) {
                // NOTE: Icky conversion -- change LevelNumber to u32?
                // NOTE: Should use total file size instead.
                if table_ids.len() <= 4 * 10usize.pow(level as u32 - 1) {
                    continue;
                }
                // Now what?  We want to kick out one table for this level.  The
                // one which overlaps the fewest child tables.
                // NOTE: A data structure for this would be nice.
                let mut smallest_overlap = usize::max_value();
                let mut smallest_overlap_table_id: TableId = 0;

                for &id in table_ids.iter() {
                    // NOTE: Pass a slice to single TableInfo element without cloning.
                    let infos: [TableInfo; 1]
                        = [self.toc.table_infos.get(&id).expect("toc valid in rebalance").clone()];
                    // NOTE: Would be nice not to allocate this vec.  Just count number of overlapping.
                    let lower_overlapping_ids: Vec<_> = Store::get_overlapping_tables(&self.toc, &infos, level + 1);
                    let overlap = lower_overlapping_ids.len();
                    // NOTE: We're biased towards releveling left-most tables given equal overlap.
                    if overlap < smallest_overlap {
                        smallest_overlap = overlap;
                        smallest_overlap_table_id = id;
                    }
                }

                assert!(smallest_overlap != usize::max_value());
                to_relevel = (level, smallest_overlap_table_id);
            } else {
                continue;
            }
            self.relevel(to_relevel.0, vec![to_relevel.1])?;
        }

        return Ok(());
    }

    // 'tables' is in order of precedence, such that frontmost tables supercede
    // later tables when merged.  (They're in reverse order by table number, if
    // in level zero.  In other levels, there's only one table, and even if there
    // was more than one, they'd have non-overlapping key ranges.)
    fn relevel<'a>(&'a mut self, level: LevelNumber, tables: Vec<TableId>) -> Result<()> {
        assert!(if level == 0 { tables.len() > 0 } else { tables.len() == 1 });
        
        // What to do:  Go to the next level, find which tables overlap.
        let table_infos: Vec<TableInfo>
            = tables.iter().map(|id| self.toc.table_infos.get(id).expect("toc valid in relevel").clone()).collect();
        let lower_overlapping_ids: Vec<TableId> = Store::get_overlapping_tables(&self.toc, &table_infos, level + 1);

        // NOTE: When releveling 0 -> 1, it's possible there are no overlapping tables.
        if lower_overlapping_ids.is_empty() && !Store::self_overlaps(&table_infos) {
            let additions: Vec<TableInfo>
                = table_infos.into_iter().map(|x: TableInfo| TableInfo{level: level, .. x}).collect();
            let entry = Entry{
                removals: tables,
                additions: additions,
            };

            append_toc(&mut self.toc, &mut self.toc_file, entry)?;
            return Ok(());
        } else {
            let mut iters: Vec<Box<MutationIterator + 'a>> = Vec::new();
            // NOTE: We might want a smarter iterator for the lower level --
            // open only one table file at a time, instead of generically
            // merging the non-overlapping tables together.

            // Add upper level's tables in 'tables' existing order (which is in order of precedence).
            // Order of lower level's tables doesn't matter, since they're non-overlapping.
            for table_id in tables.iter().chain(lower_overlapping_ids.iter()) {
                let lower_bound = Bound::Unbounded;
                self.add_table_iter_to_iters(&mut iters, *table_id, &lower_bound)?;
            }

            let mut iter = MergeIterator::make(iters)?;

            // Now we've got a store iter.  Iterate the store iter, building a set of tables.

            let mut additions: Vec<TableInfo> = Vec::new();

            'outer: loop {
                let mut builder = TableBuilder::new();
                'inner: loop {
                    if let Some(key) = iter.current_key()? {
                        let mutation = iter.current_value()?.expect("mutation given key in relevel");
                        builder.add_mutation(&key, &mutation);
                        iter.step()?;
                        if builder.file_size() > self.threshold {
                            break 'inner;
                        }
                    } else {
                        if builder.is_empty() {
                            break 'outer;
                        } else {
                            break 'inner;
                        }
                    }
                }

                // We've got a non-empty builder.  Flush it to disk.
                let table_id = self.toc.next_table_id;
                self.toc.next_table_id += 1;

                let mut f = std::fs::File::create(table_filepath(&self.directory, table_id))?;
                let (keys_offset, file_size, smallest, biggest) = builder.finish(&mut f)?;
                additions.push(TableInfo{
                    id: table_id,
                    level: level + 1,
                    keys_offset: keys_offset,
                    file_size: file_size,
                    smallest_key: smallest,
                    biggest_key: biggest,
                });
            }

            let removals: Vec<TableId>
                = tables.iter().chain(lower_overlapping_ids.iter()).map(|&x| x).collect();

            let entry = Entry{
                additions: additions,
                removals: removals,
            };

            append_toc(&mut self.toc, &mut self.toc_file, entry)?;
            // NOTE: Delete old files when releveling.

            return Ok(());
        }
    }

    fn self_overlaps(xs: &[TableInfo]) -> bool {
        for i in 0..xs.len() {
            for j in i+1..xs.len() {
                if Store::tables_overlap(&xs[i], &xs[j]) {
                    return true;
                }
            }
        }
        return false;
    }

    fn tables_overlap(x: &TableInfo, y: &TableInfo) -> bool {
        return !(x.biggest_key < y.smallest_key || y.biggest_key < x.smallest_key);
    }

    // NOTE: We'd like a better data structure for organizing a level's table by keys.
    fn get_overlapping_tables(toc: &TOC, tables: &[TableInfo], level: LevelNumber) -> Vec<TableId> {
        if let Some(level_tables) = toc.level_infos.get(&level) {
            let mut ret: Vec<TableId> = Vec::new();
            for id in level_tables {
                for info in tables {
                    if Store::tables_overlap(toc.table_infos.get(id).expect("toc valid in get_overlapping_tables"), info) {
                        ret.push(*id);
                        break;
                    }
                }
            }
            return ret;
        } else {
            return Vec::new();
        }
    }

    fn consider_split(&mut self) -> Result<()> {
        if self.memstores[0].mem_usage >= self.threshold {
            self.flush()?;
        }
        return Ok(());
    }

    fn flush_and_record(&mut self, level: LevelNumber, ms: &MemStore) -> Result<()> {
        if ms.entries.is_empty() {
            return Ok(());
        }
        let table_id = self.toc.next_table_id;
        self.toc.next_table_id += 1;
        let (keys_offset, file_size, smallest, biggest) = flush_to_disk(&self.directory, table_id, &ms)?;
        let ti = TableInfo{
            id: table_id,
            level: level,
            keys_offset: keys_offset,
            file_size: file_size,
            smallest_key: smallest,
            biggest_key: biggest,
        };
        append_toc(&mut self.toc, &mut self.toc_file, Entry{additions: vec![ti], removals: vec![]})?;
        return Ok(());
    }

    pub fn exists(&mut self, key: &str) -> bool {
        for store in self.memstores.iter() {
            if let Some(m) = store.lookup(key) {
                return match m {
                    &Mutation::Set(_) => true,
                    &Mutation::Delete => false,
                };
            }
        }
        // NOTE: We're still using a big fat memstore, so we only get to here with keys we
        // never used.
        for (_level, table_ids) in self.toc.level_infos.iter() {
            // For level zero, we want to iterate tables in reverse order.
            for table_id in table_ids.iter().rev() {
                let ti: &TableInfo = self.toc.table_infos.get(table_id).expect("invalid toc");
                if key >= &ti.smallest_key && key <= &ti.biggest_key {
                    // NOTE: We'll want to use exists_table.
                    let opt_mut = lookup_table(&self.directory, ti, key).unwrap();  // NOTE error handling
                    if let Some(m) = opt_mut {
                        return match m {
                            Mutation::Set(_) => true,
                            Mutation::Delete => false,
                        }
                    }
                }

            }
        }

        return false;
    }

    pub fn get(&mut self, key: &str) -> Option<String> {
        for store in self.memstores.iter() {
            if let Some(m) = store.lookup(key) {
                return match m {
                    &Mutation::Set(ref x) => Some(x.clone()),
                    &Mutation::Delete => None,
                }
            }
        }
        // NOTE: We're still using a big fat memstore, so we only get to this code with keys we never used.
        for (_level, table_ids) in self.toc.level_infos.iter() {
            // For level zero, we want to iterate tables in reverse order.
            // NOTE: For other levels, we don't want to iterate at all.  Too much CPU.
            for table_id in table_ids.iter().rev() {
                let ti: &TableInfo = self.toc.table_infos.get(table_id).expect("invalid toc");
                if key >= &ti.smallest_key && key <= &ti.biggest_key {
                    let opt_mut = lookup_table(&self.directory, ti, key).unwrap();  // NOTE error handling
                    if let Some(m) = opt_mut {
                        return match m {
                            Mutation::Set(x) => Some(x),
                            Mutation::Delete => None,
                        }
                    }
                }
            }
        }

        return None;
    }

    fn add_table_iter_to_iters<'a>(
        &self, iters: &mut Vec<Box<MutationIterator + 'a>>, table_id: TableId, lower_bound: &Bound<String>)
        -> Result<()> {
        let ti: &TableInfo = self.toc.table_infos.get(&table_id).expect("invalid toc");
        // NOTE: Don't clone the lower bound every freaking time.
        let interval = Interval::<String>{lower: lower_bound.clone(), upper: Bound::Unbounded};
        let iter = TableIterator::make(&self.directory, ti, &interval)?;
        iters.push(Box::new(iter));
        return Ok(());
    }

    // NOTE: Add directional ranges (i.e. backwards range iteration).
    // NOTE: If the StoreIter keeps self borrowed, it should hold a reference to self that we can use
    // to iterate.
    pub fn range<'a>(&'a self, interval: &Interval<String>) -> Result<StoreIter<'a>> {
        // NOTE: Could short-circuit for empty/one-key interval.
        let mut iters: Vec<Box<MutationIterator + 'a>> = Vec::new();
        for store in self.memstores.iter() {
            iters.push(Box::new(MemStoreIterator::<'a>::make(store, interval)));
        }

        for (level, table_ids) in self.toc.level_infos.iter() {
            if *level == 0 {
                // Tables overlap, add them in reverse order.
                for table_id in table_ids.iter().rev() {
                    // NOTE: We could check if the intervals actually overlap.
                    self.add_table_iter_to_iters(&mut iters, *table_id, &interval.lower)?;
                }
            } else {
                // NOTE: We should only add those tables with relevant documents.  And iterate in order.
                // Order doesn't matter for now because it describes key precedence.
                for table_id in table_ids.iter() {
                    self.add_table_iter_to_iters(&mut iters, *table_id, &interval.lower)?;
                }
            }
        }

        return Ok(StoreIter{
            interval: interval.clone(),
            iters: MergeIterator::make(iters)?,
        });
    }

    pub fn next(&self, iter: &mut StoreIter) -> Result<Option<(String, String)>> {
        loop {
            if let Some(key) = iter.iters.current_key()? {
                if !below_upper_bound(&key, &iter.interval.upper) {
                    return Ok(None);
                }
                let mutation: Mutation = iter.iters.current_value()?.expect("a current_value");
                iter.iters.step()?;
                match mutation {
                    Mutation::Set(value) => {
                        return Ok(Some((key, value)));
                    },
                    Mutation::Delete => {
                        continue;
                    }
                }
            } else {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::Bound;
    use super::*;

    use rand::*;

    struct TestStore {
        // In an option so we can drop it before deleting directory.
        store: Option<Store>,
        directory: String,
    }

    fn random_testdir() -> String {
        let mut rng = rand::thread_rng();
        let mut x: u32 = rng.gen();
        let mut ret = "testdir-".to_string();
        for _ in 0..6 {
            ret.push(std::char::from_u32(97 + (x % 26)).unwrap());
            x /= 26;
        }
        return ret;
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            // Cleanup the Store before we delete the directory.
            self.close();
            std::fs::remove_dir_all(&self.directory).expect("remove_dir_all");
        }
    }

    impl TestStore {
        fn create() -> TestStore {
            let dir: String = random_testdir();
            Store::create(&dir).unwrap();
            let mut ts = TestStore{store: None, directory: dir};
            ts.open();
            return ts;
        }
        fn open(&mut self) {
            assert!(self.store.is_none());
            let store: Store = Store::open(&self.directory, 100).unwrap();
            self.store = Some(store);
        }
        fn close(&mut self) -> Option<()> {
            return self.store.take().map(|_| ());
        }
        fn kv(&mut self) -> &mut Store {
            return self.store.as_mut().unwrap();
        }
    }

    #[test]
    fn putget() {
        let mut ts = TestStore::create();
        let kv = ts.kv();
        kv.put("foo", "Hey").unwrap();
        let x: Option<String> = kv.get("foo");
        assert_eq!(Some("Hey".to_string()), x);
        assert!(kv.exists("foo"));
        assert_eq!(None, kv.get("bar"));
        assert!(!kv.exists("bar"));
    }

    #[test]
    fn range() {
        let mut ts = TestStore::create();
        let kv = ts.kv();
        kv.put("a", "alpha").unwrap();
        kv.put("b", "beta").unwrap();
        kv.put("c", "charlie").unwrap();
        kv.put("d", "delta").unwrap();
        let interval = Interval::<String>{lower: Bound::Unbounded, upper: Bound::Excluded("d".to_string())};
        let mut it: StoreIter = kv.range(&interval).expect("range");
        assert_eq!(Some(("a".to_string(), "alpha".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(Some(("b".to_string(), "beta".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(Some(("c".to_string(), "charlie".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(None, kv.next(&mut it).unwrap());
    }

    #[test]
    fn overwrite() {
        let mut ts = TestStore::create();
        let kv = ts.kv();

        kv.put("a", "alpha").unwrap();
        kv.put("a", "alpha-2").unwrap();
        assert_eq!(Some("alpha-2".to_string()), kv.get("a"));
        let inserted: bool = kv.insert("a", "alpha-3").unwrap();
        assert!(!inserted);
        let overwrote: bool = kv.replace("a", "alpha-4").unwrap();
        assert!(overwrote);
        assert_eq!(Some("alpha-4".to_string()), kv.get("a"));
    }

    fn write_basic_kv(ts: &mut TestStore) {
        let kv = ts.kv();
        for i in 0..102 {
            kv.put(&i.to_string(), &format!("value-{}", i.to_string())).unwrap();
        }
        // Remove one, so that we test Delete entries really do override Set entries.
        let removed: bool = kv.remove("11").unwrap();
        assert!(removed);
        assert!(1 < kv.memstores.len());
    }

    fn verify_basic_kv(ts: &mut TestStore) {
        let kv = ts.kv();
        let interval = Interval::<String>{lower: Bound::Excluded("1".to_string()), upper: Bound::Unbounded};
        let mut it: StoreIter = kv.range(&interval).expect("range");
        assert_eq!(Some(("10".to_string(), "value-10".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(Some(("100".to_string(), "value-100".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(Some(("101".to_string(), "value-101".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(Some(("12".to_string(), "value-12".to_string())), kv.next(&mut it).unwrap());
        assert_eq!(Some(("13".to_string(), "value-13".to_string())), kv.next(&mut it).unwrap());
    }

    #[test]
    fn many() {
        let mut ts = TestStore::create();
        write_basic_kv(&mut ts);
        verify_basic_kv(&mut ts);
    }

    #[test]
    fn disk() {
        let mut ts = TestStore::create();
        write_basic_kv(&mut ts);
        ts.kv().flush().unwrap();
        // Remove (and drop) existing store.
        assert!(ts.close().is_some());
        ts.open();
        verify_basic_kv(&mut ts);
    }

    #[test]
    fn disk_missing_key() {
        let mut ts = TestStore::create();
        write_basic_kv(&mut ts);
        ts.kv().flush().unwrap();
        // Remove (and drop) existing store.
        assert!(ts.close().is_some());
        ts.open();
        // This actually hits the disk, because the key has no reference in the memstores.
        assert_eq!(None, ts.kv().get("bogus"));
    }
}
