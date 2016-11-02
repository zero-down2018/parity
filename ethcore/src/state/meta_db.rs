// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity. If not, see <http://www.gnu.org/licenses/>.

//! Account meta-database.
//!
//! This is a journalled database which stores information on accounts.
//! It is implemented using a configurable journal (following a similar API to JournalDB)
//! which builds off of an on-disk flat representation of the state for the last committed block.
//!
//! Any query about an account can be definitively answered for any block in the journal
//! or the canonical base.
//!
//! The journal format is two-part. First, for every era we store a list of
//! candidate hashes.
//!
//! For each hash, we store a list of changes in that candidate.

use util::{Address, HeapSizeOf, H256, U256, RwLock};
use util::kvdb::{Database, DBTransaction};
use rlp::{Decoder, DecoderError, RlpDecodable, RlpEncodable, RlpStream, Stream, Rlp, View};

use std::collections::{BTreeMap, HashMap, BTreeSet};
use std::sync::Arc;

const PADDING: [u8; 10] = [0; 10];

// generate a key for the given era.
fn journal_key(era: &u64) -> Vec<u8> {
	let mut stream = RlpStream::new_list(3);
	stream.append(&"journal").append(era).append(&&PADDING[..]);
	stream.out()
}

// generate a key for the given id.
fn id_key(id: &H256) -> Vec<u8> {
	let mut stream = RlpStream::new_list(3);
	stream.append(&"journal").append(id).append(&&PADDING[..]);
	stream.out()
}

/// Errors which can occur in the operation of the meta db.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
	/// A database error.
	Database(String),
	/// No journal entry found for the specified era, id.
	MissingJournalEntry(u64, H256),
	/// Request made for pruned state.
	StatePruned(u64, H256),
}

/// Account meta-information.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AccountMeta {
	/// The size of this account's code.
	pub code_size: usize,
	/// The hash of this account's code.
	pub code_hash: H256,
	/// Storage root for the trie.
	pub storage_root: H256,
	/// Account balance.
	pub balance: U256,
	/// Account nonce.
	pub nonce: U256,
}

known_heap_size!(0, AccountMeta);

impl RlpEncodable for AccountMeta {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(5)
			.append(&self.code_size)
			.append(&self.code_hash)
			.append(&self.storage_root)
			.append(&self.balance)
			.append(&self.nonce);
	}
}

impl RlpDecodable for AccountMeta {
	fn decode<D>(decoder: &D) -> Result<Self, DecoderError> where D: Decoder {
		let rlp = decoder.as_rlp();

		Ok(AccountMeta {
			code_size: try!(rlp.val_at(0)),
			code_hash: try!(rlp.val_at(1)),
			storage_root: try!(rlp.val_at(2)),
			balance: try!(rlp.val_at(3)),
			nonce: try!(rlp.val_at(4)),
		})
	}
}

// Each journal entry stores the parent hash of the block it corresponds to
// and the changes in the meta state it lead to.
#[derive(Debug, PartialEq)]
struct JournalEntry {
	parent: H256,
	// every entry which was set for this era.
	entries: HashMap<Address, Option<AccountMeta>>,
}

impl HeapSizeOf for JournalEntry {
	fn heap_size_of_children(&self) -> usize {
		self.entries.heap_size_of_children()
	}
}

impl RlpEncodable for JournalEntry {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(2);
		s.append(&self.parent);

		s.begin_list(self.entries.len());
		for (addr, delta) in self.entries.iter() {
			s.begin_list(2).append(addr);
			s.begin_list(2);

			match *delta {
				Some(ref new) => {
					s.append(&true).append(new);
				}
				None => {
					s.append(&false).append_empty_data();
				}
			}
		}
	}
}

impl RlpDecodable for JournalEntry {
	fn decode<D>(decoder: &D) -> Result<Self, DecoderError> where D: Decoder {
		let rlp = decoder.as_rlp();
		let mut entries = HashMap::new();

		for entry in try!(rlp.at(1)).iter() {
			let addr = try!(entry.val_at(0));
			let maybe = try!(entry.at(1));

			let delta = match try!(maybe.val_at(0)) {
				true => Some(try!(maybe.val_at(1))),
				false => None,
			};

			entries.insert(addr, delta);
		}

		Ok(JournalEntry {
			parent: try!(rlp.val_at(0)),
			entries: entries,
		})
	}
}

// The journal used to store meta info.
// Invariants which must be preserved:
//   - The parent entry of any given journal entry must also be present
//     in the journal, unless it's the canonical base being built off of.
//   - No cyclic entries. There should never be a path from any given entry to
//     itself other than the empty path.
//   - Modifications may only point to entries in the journal.
#[derive(Debug, PartialEq)]
struct Journal {
	// maps era, id pairs to potential canonical meta info.
	entries: BTreeMap<(u64, H256), JournalEntry>,
	// maps addresses to sets of blocks they were modified at.
	modifications: HashMap<Address, BTreeSet<(u64, H256)>>,
	canon_base: (u64, H256), // the base which the journal builds off of.
}

impl Journal {
	// read the journal from the database, starting from the last committed
	// era.
	fn read_from(db: &Database, col: Option<u32>, base: (u64, H256)) -> Result<Self, String> {
		trace!(target: "meta_db", "loading journal");

		let mut journal = Journal {
			entries: BTreeMap::new(),
			modifications: HashMap::new(),
			canon_base: base,
		};

		let mut era = base.0 + 1;
		while let Some(hashes) = try!(db.get(col, &journal_key(&era))).map(|x| ::rlp::decode::<Vec<H256>>(&x)) {
			let candidates: Result<HashMap<_, _>, String> = hashes.into_iter().map(|hash| {
				let journal_rlp = try!(db.get(col, &id_key(&hash)))
					.expect(&format!("corrupted database: missing journal data for {}.", hash));

				let entry: JournalEntry = ::rlp::decode(&journal_rlp);

				for addr in entry.entries.keys() {
					journal.modifications.entry(*addr).or_insert_with(BTreeSet::new).insert((era, hash));
				}

				Ok(((era, hash), entry))
			}).collect();
			let candidates = try!(candidates);

			trace!(target: "meta_db", "journal: loaded {} candidates for era {}", candidates.len(), era);
			journal.entries.extend(candidates);
			era += 1;
		}

		Ok(journal)
	}

	// write journal era.
	fn write_era(&self, col: Option<u32>, batch: &mut DBTransaction, era: u64) {
		let key = journal_key(&era);
		let candidate_hashes: Vec<_> = self.entries.keys()
			.skip_while(|&&(ref e, _)| e < &era)
			.take_while(|&&(e, _)| e == era)
			.map(|&(_, ref h)| h.clone())
			.collect();

		batch.put(col, &key, &*::rlp::encode(&candidate_hashes));
	}
}

impl HeapSizeOf for Journal {
	fn heap_size_of_children(&self) -> usize {
		self.entries.heap_size_of_children()
			// + self.modifications.heap_size_of_children()
			// ^~~ uncomment when BTreeSet has a HeapSizeOf implementation.
	}
}

/// The account meta-database. See the module docs for more details.
/// It can't be queried without a `MetaBranch` which allows for accurate
/// queries along the current branch.
///
/// This has a short journal period, and is only really usable while syncing.
/// When replaying old transactions, it can't be used reliably.
#[derive(Clone)]
pub struct MetaDB {
	col: Option<u32>,
	db: Arc<Database>,
	journal: Arc<RwLock<Journal>>,
	overlay: HashMap<Address, Option<AccountMeta>>,
}

impl MetaDB {
	/// Create a new `MetaDB` from a database and column. This will also load the journal.
	///
	/// After creation, check the last committed era to see if the genesis state
	/// is in. If not, it should be inserted, journalled, and marked canonical.
	pub fn new(db: Arc<Database>, col: Option<u32>, genesis_hash: &H256) -> Result<Self, String> {
		let base: (u64, H256) = try!(db.get(col, b"base")).map(|raw| {
			let rlp = Rlp::new(&raw);

			(rlp.val_at(0), rlp.val_at(1))
		}).unwrap_or_else(|| (0, genesis_hash.clone()));

		let journal = try!(Journal::read_from(&*db, col, base));

		Ok(MetaDB {
			col: col,
			db: db,
			journal: Arc::new(RwLock::new(journal)),
			overlay: HashMap::new(),
		})
	}

	/// Journal all pending changes under the given era and id.
	pub fn journal_under(&mut self, batch: &mut DBTransaction, now: u64, id: H256, parent_id: H256) {
		trace!(target: "meta_db", "journalling ({}, {})", now, id);
		let mut journal = self.journal.write();

		let j_entry = JournalEntry {
			parent: parent_id,
			entries: ::std::mem::replace(&mut self.overlay, HashMap::new()),
		};

		for addr in j_entry.entries.keys() {
			journal.modifications.entry(*addr).or_insert_with(BTreeSet::new).insert((now, id));
		}

		let encoded = ::rlp::encode(&j_entry);

		trace!(target: "meta_db", "produced entry: {:?}", &*encoded);

		batch.put(self.col, &id_key(&id), &encoded);

		journal.entries.insert((now, id), j_entry);
		journal.write_era(self.col, batch, now);
	}

	/// Mark a candidate for an era as canonical, applying its changes
	/// and invalidating its siblings.
	pub fn mark_canonical(&mut self, batch: &mut DBTransaction, end_era: u64, canon_id: H256) {


		trace!(target: "meta_db", "mark_canonical: ({}, {})", end_era, canon_id);
		let mut journal = self.journal.write();

		let candidate_hashes: Vec<_> = journal.entries.keys()
			.skip_while(|&&(ref e, _)| e < &end_era)
			.take_while(|&&(e, _)| e == end_era)
			.map(|&(_, ref h)| h.clone())
			.collect();

		for id in candidate_hashes {
			let entry = journal.entries.remove(&(end_era, id)).expect("entries known to contain this key; qed");
			batch.delete(self.col, &id_key(&id));

			// remove modifications entries.
			for addr in entry.entries.keys() {
				let remove = match journal.modifications.get_mut(addr) {
					Some(ref mut mods) => {
						mods.remove(&(end_era, id));
						mods.is_empty()
					}
					None => false,
				};

				if remove {
					journal.modifications.remove(addr);
				}
			}

			// apply canonical changes.
			if id == canon_id {
				for (addr, delta) in entry.entries {
					match delta {
						Some(delta) => batch.put(self.col, &addr, &*::rlp::encode(&delta)),
						None => batch.delete(self.col, &addr),
					}
				}
			}
		}

		journal.canon_base = (end_era, canon_id);

		// update meta keys in the database.
		let mut base_stream = RlpStream::new_list(2);
		base_stream.append(&journal.canon_base.0).append(&journal.canon_base.1);

		batch.put(self.col, b"base", &*base_stream.drain());
		batch.delete(self.col, &journal_key(&end_era));
	}

	/// Query the state of an account at a given block. A return value
	/// of `None` means that the account definitively does not exist on this branch.
	/// This will query the overlay of pending changes first.
	///
	/// Will fail on database error, state pruned, or unexpected missing journal entry.
	pub fn get(&self, address: &Address, at: (u64, H256)) -> Result<Option<AccountMeta>, Error> {
		trace!(target: "meta_db", "get: {:?} at {:?}", address, at);

		let get_from_db = || match self.db.get(self.col, &*address) {
			Ok(meta) => Ok(meta.map(|x| ::rlp::decode(&x))),
			Err(e) => Err(Error::Database(e)),
		};

		if let Some(meta) = self.overlay.get(address) {
			return Ok(meta.clone());
		}

		let journal = self.journal.read();

		// fast path for base query.
		if at == journal.canon_base {
			return get_from_db();
		}

		let (mut era, mut id) = at;
		let mut entry = try!(journal.entries.get(&(era, id)).ok_or_else(|| Error::MissingJournalEntry(era, id)));

		// iterate the modifications for this account in reverse order (by id),
		for &(mod_era, ref mod_id) in journal.modifications.get(address).into_iter().flat_map(|m| m.iter().rev()) {
			if era <= journal.canon_base.0 { break }

			// walk the relevant path down the journal backwards until we're aligned with
			// the era
			while era > mod_era {
				id = entry.parent;
				era -= 1;
				entry = try!(journal.entries.get(&(era, id)).ok_or_else(|| Error::MissingJournalEntry(era, id)));
			}

			// then continue until we reach the right ID or have to traverse further down.
			if mod_id != &id { continue }

			assert_eq!((era, &id), (mod_era, mod_id), "journal traversal led to wrong entry");
			return Ok(entry.entries.get(address)
				.expect("modifications set always contains correct entries; qed")
				.clone());
		}

		if era <= journal.canon_base.0 && id != journal.canon_base.1 {
			return Err(Error::StatePruned(era, id));
		}

		// no known modifications -- fetch from database.
		get_from_db()
	}

	/// Set the given account's details on this address in the pending changes
	/// overlay.
	/// This will overwrite any previous changes to the overlay,
	/// and will be queried prior to the journal.
	pub fn set(&mut self, address: Address, meta: AccountMeta) {
		trace!(target: "meta_db", "set({:?}, {:?})", address, meta);
		self.overlay.insert(address, Some(meta));
	}

	/// Destroy the account details here.
	pub fn remove(&mut self, address: Address) {
		trace!(target: "meta_db", "remove({:?})", address);
		self.overlay.insert(address, None);
	}
}

impl HeapSizeOf for MetaDB {
	fn heap_size_of_children(&self) -> usize {
		self.overlay.heap_size_of_children() + self.journal.read().heap_size_of_children()
	}
}

#[cfg(test)]
mod tests {
	use super::{AccountMeta, MetaDB};
	use devtools::RandomTempPath;

	use util::{U256, H256};
	use util::kvdb::Database;

	use std::sync::Arc;

	#[test]
	fn loads_journal() {
		let path = RandomTempPath::create_dir();
		let db = Arc::new(Database::open_default(&*path.as_path().to_string_lossy()).unwrap());
		let mut meta_db = MetaDB::new(db.clone(), None, &Default::default()).unwrap();

		for i in 0..10u64 {
			let this = U256::from(i + 1);
			let parent = U256::from(i);

			let mut batch = db.transaction();
			meta_db.journal_under(&mut batch, i + 1, this.into(), parent.into());
			db.write(batch).unwrap();
		}

		let mut batch = db.transaction();
		meta_db.mark_canonical(&mut batch, 1, U256::from(1).into());
		db.write(batch).unwrap();

		let journal = meta_db.journal;
		let meta_db = MetaDB::new(db.clone(), None, &Default::default()).unwrap();

		assert_eq!(&*journal.read(), &*meta_db.journal.read());
	}
}