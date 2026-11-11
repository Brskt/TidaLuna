use super::catalog::{lost_rows, write};
use super::swap::replace;
use crate::db::ROUTINE_SYNC;
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use std::path::Path;

/// A table of the store and the schema version that added it. A file stamped below `since` was
/// written before the table existed: its absence is no loss. Version 0 is the schema of every
/// build published before the stamp, and a file with no stamp reads 0.
struct Table {
    name: &'static str,
    since: i64,
    create: &'static str,
}

const SETTINGS: Table = Table {
    name: "settings",
    since: 0,
    create: "CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
};

const NATIVE_TRUST: Table = Table {
    name: "native_trust",
    since: 0,
    create: "CREATE TABLE IF NOT EXISTS native_trust (
        code_hash TEXT NOT NULL,
        plugin    TEXT NOT NULL,
        module    TEXT NOT NULL,
        granted   INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (code_hash, plugin, module)
    )",
};

const REPAIR_NOTICE: Table = Table {
    name: "repair_notice",
    since: 1,
    create: "CREATE TABLE IF NOT EXISTS repair_notice (
        id     INTEGER PRIMARY KEY CHECK (id = 1),
        notice TEXT NOT NULL
    )",
};

/// Every table of the store: a new one is declared here, with the version it brings.
const TABLES: [&Table; 3] = [&SETTINGS, &NATIVE_TRUST, &REPAIR_NOTICE];

/// What `init_schema` stamps in `user_version`: the newest version any table was added in.
const SCHEMA_VERSION: i64 = {
    let mut newest = 0;
    let mut at = 0;
    while at < TABLES.len() {
        if TABLES[at].since > newest {
            newest = TABLES[at].since;
        }
        at += 1;
    }
    newest
};

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

pub(crate) fn init_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", ROUTINE_SYNC)?;

    // The stamp lands with the tables it vouches for, or not at all. The write lock is taken up
    // front when the stamp is owed: a snapshot that began as a read cannot be upgraded once another
    // writer has committed.
    let stamped = schema_version(conn)?;
    let owed = stamped < SCHEMA_VERSION;
    let tx = conn.transaction_with_behavior(if owed {
        TransactionBehavior::Immediate
    } else {
        TransactionBehavior::Deferred
    })?;
    for table in TABLES {
        tx.execute(table.create, [])?;
    }
    if owed {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()
}

/// What opening the store found. Each outcome is announced differently, hence named.
/// `turned_off` lists the settings a possible loss wrote down as their `unreadable` value.
#[derive(Debug, PartialEq)]
pub(crate) enum Opened {
    Healthy,
    /// Only an index disagreed with its table: rebuilt from the table. `trust` is what the table
    /// had already lost, which rebuilding an index cannot bring back.
    Reindexed {
        turned_off: Vec<&'static str>,
        trust: Trust,
    },
    /// Rebuilt from the settings rows still readable, a copy of the damaged file kept beside it.
    Rebuilt {
        turned_off: Vec<&'static str>,
        trust: Trust,
    },
    /// The rebuilt file could not be put in place: the damaged one is where it was, and this run
    /// keeps its settings in memory.
    Volatile {
        turned_off: Vec<&'static str>,
        trust: Trust,
    },
}

/// What a rebuild did to the `native_trust` decisions, which it never carries over. Read from the
/// damaged file before anything is rewritten; the banner states a loss only when there was one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Trust {
    /// No decision was lost: the table held none, or a repair kept it.
    Empty,
    /// The table held decisions: native plugins ask again.
    Dropped,
    /// The table could not be read: native plugins may ask again.
    Unknown,
}

/// Both trees, since either can lie alone: a stale empty table page answers no rows while its
/// index still lists them. A read that fails is `Unknown`, never `Empty`.
fn trust_at_open(conn: &Connection) -> Trust {
    let found = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_trust NOT INDEXED)
             OR EXISTS(SELECT 1 FROM native_trust INDEXED BY sqlite_autoindex_native_trust_1)",
        [],
        |row| row.get::<_, bool>(0),
    );
    match found {
        Ok(true) => Trust::Dropped,
        Ok(false) => Trust::Empty,
        Err(_) => Trust::Unknown,
    }
}

/// What `REINDEX` cannot give back. It rebuilds an index from its table; only a row the index
/// lists and the table no longer holds is gone: a row that merely exists would announce a loss on
/// every healthy table. Read before it runs, since afterwards the two agree on the loss. A read
/// that fails is `Unknown`, never `Empty`. `count(*)` is answered from the table whatever index is
/// named; `count(code_hash)` walks the index.
fn trust_lost_to_reindex(conn: &Connection) -> Trust {
    let counts = conn.query_row(
        "SELECT (SELECT count(code_hash) FROM native_trust NOT INDEXED),
                (SELECT count(code_hash) FROM native_trust INDEXED BY sqlite_autoindex_native_trust_1)",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    );
    match counts {
        Ok((table, index)) if index > table => Trust::Dropped,
        Ok(_) => Trust::Empty,
        Err(_) => Trust::Unknown,
    }
}

/// What the user is told about a repair, in the page's banner.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairNotice {
    turned_off: Vec<&'static str>,
    trust: Trust,
    /// Changes reach the disk again.
    saved: bool,
}

impl Opened {
    /// `None` when nothing the user would notice changed.
    pub(crate) fn notice(&self) -> Option<RepairNotice> {
        let (turned_off, trust, saved) = match self {
            Self::Healthy => return None,
            Self::Reindexed {
                turned_off,
                trust: Trust::Empty,
            } if turned_off.is_empty() => return None,
            Self::Reindexed { turned_off, trust } | Self::Rebuilt { turned_off, trust } => {
                (turned_off, *trust, true)
            }
            Self::Volatile { turned_off, trust } => (turned_off, *trust, false),
        };
        Some(RepairNotice {
            turned_off: turned_off.clone(),
            trust,
            saved,
        })
    }
}

/// What the `repair_notice` row holds. The keys come back as the declared names: one this build
/// does not declare cannot be told to the user.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNotice {
    turned_off: Vec<String>,
    trust: Trust,
    saved: bool,
}

impl Trust {
    /// The wider of two losses: a decision known dropped over one that may be, over none.
    fn widest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Dropped, _) | (_, Self::Dropped) => Self::Dropped,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Empty,
        }
    }
}

/// The repair the store held a notice of when a new one is made.
enum Earlier {
    Nothing,
    Told(RepairNotice),
    /// A notice was there, or may have been, and what it said is gone.
    Unreadable,
}

/// The settings reading as a loss writes them down: what an earlier repair turned off, or what
/// the user chose to turn off, which a row cannot tell apart. A row that does not read counts, as
/// it reads as that value too.
fn marked_off(store: &Connection) -> Vec<&'static str> {
    lost_rows()
        .into_iter()
        .filter(|(key, row)| {
            match store
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    params![key],
                    |found| found.get::<_, String>(0),
                )
                .optional()
            {
                Ok(found) => found.as_ref() == Some(row),
                Err(_) => true,
            }
        })
        .map(|(key, _)| key)
        .collect()
}

impl RepairNotice {
    /// This repair told together with the one the user has not yet been told of.
    fn after(mut self, earlier: &Earlier, store: &Connection) -> Self {
        match earlier {
            Earlier::Nothing => {}
            Earlier::Told(earlier) => {
                for key in &earlier.turned_off {
                    if !self.turned_off.contains(key) {
                        self.turned_off.push(key);
                    }
                }
                self.trust = self.trust.widest(earlier.trust);
            }
            Earlier::Unreadable => {
                for key in marked_off(store) {
                    if !self.turned_off.contains(&key) {
                        self.turned_off.push(key);
                    }
                }
                self.trust = self.trust.widest(Trust::Unknown);
            }
        }
        self
    }

    /// All that can still be told of a notice that cannot be read: every setting that reads as a
    /// repair leaves it, and permissions that may be asked again.
    fn unreadable_earlier(store: &Connection) -> Self {
        Self {
            turned_off: marked_off(store),
            trust: Trust::Unknown,
            saved: true,
        }
    }
}

/// The notice a repair is about to replace the store of. A read that fails here is a lost fact,
/// not a missing one: the repair renames a new file over the one that held it.
fn earlier_notice(conn: &Connection, table_vanished: bool) -> Earlier {
    if table_vanished {
        crate::verr!("[SETTINGS] the repair notice table left the schema, told as a wider loss");
        return Earlier::Unreadable;
    }
    match read_notice(conn) {
        Ok(Some(notice)) => Earlier::Told(notice),
        Ok(None) => Earlier::Nothing,
        Err(e) => {
            crate::verr!(
                "[SETTINGS] an earlier repair notice cannot be read, told as a wider loss: {e}"
            );
            Earlier::Unreadable
        }
    }
}

/// Writes what `opened` owes the user into the store it describes, with `earlier`, a repair never
/// told of, folded in. The row travels with the repair: inside the transaction that makes a
/// reindex real, inside the file a rebuild renames into place. It stays until the page confirms
/// the banner is on the screen.
fn record_notice(conn: &Connection, opened: &Opened, earlier: &Earlier) -> rusqlite::Result<()> {
    let notice = match (opened.notice(), earlier) {
        (Some(notice), earlier) => notice.after(earlier, conn),
        // This repair tells nothing, but the row an earlier one left is unreadable: replaced, not
        // left to fail again at every launch.
        (None, Earlier::Unreadable) => RepairNotice::unreadable_earlier(conn),
        (None, _) => return Ok(()),
    };
    let json = serde_json::to_string(&notice)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT OR REPLACE INTO repair_notice (id, notice) VALUES (1, ?1)",
        params![json],
    )
    .map(drop)
}

fn read_notice(conn: &Connection) -> rusqlite::Result<Option<RepairNotice>> {
    let Some(json) = conn
        .query_row("SELECT notice FROM repair_notice WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
    else {
        return Ok(None);
    };
    let stored: StoredNotice = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let declared: Vec<&'static str> = lost_rows().into_iter().map(|(key, _)| key).collect();
    Ok(Some(RepairNotice {
        turned_off: declared
            .into_iter()
            .filter(|key| stored.turned_off.iter().any(|named| named == key))
            .collect(),
        trust: stored.trust,
        saved: stored.saved,
    }))
}

/// A read that fails for what the row holds or for damage fails the same way at every
/// launch; a busy or an I/O error is gone at the next.
fn permanently_unreadable(e: &rusqlite::Error) -> bool {
    corruption(e)
        || matches!(
            e,
            rusqlite::Error::FromSqlConversionFailure(..) | rusqlite::Error::InvalidColumnType(..)
        )
}

/// The repair the user has not been told of yet, from this launch or an earlier one. A row that
/// can never be read again is told as a wider loss: answering nothing would owe it for good.
pub(crate) fn pending_notice(conn: &Connection) -> Option<RepairNotice> {
    match read_notice(conn) {
        Ok(notice) => notice,
        Err(e) if permanently_unreadable(&e) => {
            crate::verr!(
                "[SETTINGS] the pending repair notice cannot be read, told as a wider loss: {e}"
            );
            Some(RepairNotice::unreadable_earlier(conn))
        }
        Err(e) => {
            crate::verr!(
                "[SETTINGS] cannot read the pending repair notice, it stays for the next ask: {e}"
            );
            None
        }
    }
}

/// The banner is on the page's screen: the notice is told, and goes.
pub(crate) fn acknowledge_repair_notice(conn: &Connection) {
    if let Err(e) = conn.execute("DELETE FROM repair_notice", []) {
        crate::verr!("[SETTINGS] cannot clear the repair notice, it will be shown again: {e}");
    }
}

pub(crate) fn corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Whether `PRAGMA integrity_check` found nothing. A check that cannot get through a damaged
/// page answers with the corruption code instead of rows, which is "not clean" as well.
fn is_clean(conn: &Connection) -> rusqlite::Result<bool> {
    let rows = conn.prepare("PRAGMA integrity_check").and_then(|mut stmt| {
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
    });
    match rows {
        Ok(rows) => Ok(rows == ["ok"]),
        Err(e) if corruption(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

struct Salvaged {
    rows: Vec<(String, String)>,
    /// False when a row could not be read, or the scan did not reach the end of the table. The
    /// copy's own outcome decides the loss: a check of the file's structure passes a value whose
    /// type flipped to a blob.
    intact: bool,
}

/// The settings rows the table itself still yields, its damaged index left out. `native_trust`
/// is not carried: a single byte (serial type 8 to 9) turns a stored refusal into a grant.
/// What a failed check touched is asked again. A row that does not read is skipped rather than
/// ending the scan; the rows after it still carry over.
fn salvage(conn: &Connection) -> Salvaged {
    let mut salvaged = Salvaged {
        rows: Vec::new(),
        intact: false,
    };
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM settings NOT INDEXED") else {
        return salvaged;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return salvaged;
    };
    let mut skipped = false;
    loop {
        match rows.next() {
            Ok(Some(row)) => match (row.get::<_, String>(0), row.get::<_, String>(1)) {
                (Ok(key), Ok(value)) => salvaged.rows.push((key, value)),
                _ => skipped = true,
            },
            Ok(None) => {
                salvaged.intact = !skipped;
                return salvaged;
            }
            Err(_) => return salvaged,
        }
    }
}

/// A file that already holds pages, was written since the table existed, and whose schema no
/// longer lists it: `init_schema` would put an empty table in its place, and an empty table passes
/// its own check. A file stamped before the table was added never had it.
fn table_vanished(conn: &Connection, table: &Table) -> rusqlite::Result<bool> {
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let listed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
        params![table.name],
        |row| row.get(0),
    )?;
    Ok(pages > 0 && listed == 0 && schema_version(conn)? >= table.since)
}

/// The tables `init_schema` would put back empty, hiding what they held from anything read after
/// it. Only `settings` stops the schema pass: the other two widen what a repair tells.
struct Vanished {
    settings: bool,
    trust: bool,
    notice: bool,
}

/// Whether the `settings` table and its index pass their own check. The full check cannot say:
/// it blames a page two trees claim on whichever it walks second, and names no tree at all for
/// some damage. A check that errors is not clean.
fn settings_clean(conn: &Connection) -> bool {
    let rows = conn
        .prepare("PRAGMA integrity_check('settings')")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        });
    matches!(rows, Ok(rows) if rows == ["ok"])
}

/// Writes down, after a possible loss, the `unreadable` value of every setting that has no row
/// and would otherwise read as `absent`: a refusal the store lost must not read as consent.
fn mark_lost(
    conn: &Connection,
    has_row: impl Fn(&str) -> bool,
) -> rusqlite::Result<Vec<&'static str>> {
    let mut turned_off = Vec::new();
    for (key, row) in lost_rows() {
        if !has_row(key) {
            write(conn, key, &row)?;
            turned_off.push(key);
        }
    }
    if !turned_off.is_empty() {
        crate::verr!(
            "[SETTINGS] settings.db may have lost rows, set to their safe value: {}",
            turned_off.join(", ")
        );
    }
    Ok(turned_off)
}

fn has_row(conn: &Connection, key: &str) -> bool {
    matches!(
        conn.query_row(
            "SELECT 1 FROM settings WHERE key = ?1",
            params![key],
            |_| { Ok(()) }
        )
        .optional(),
        Ok(Some(()))
    )
}

/// The new store, whole, in one transaction: the carried rows and the markers of what is lost
/// land together or not at all.
fn populate(
    conn: &mut Connection,
    carried: &[(String, String)],
    settings_lost: bool,
) -> rusqlite::Result<Vec<&'static str>> {
    init_schema(conn)?;
    let tx = conn.transaction()?;
    for (key, value) in carried {
        write(&tx, key, value)?;
    }
    let turned_off = if settings_lost {
        mark_lost(&tx, |key| carried.iter().any(|(carried, _)| carried == key))?
    } else {
        Vec::new()
    };
    tx.commit()?;
    Ok(turned_off)
}

/// The new store is built in memory, where nothing can fail on a disk, then put in place. When
/// that fails, the damaged file is still where it was, and this run keeps the same store in memory.
fn rebuild(
    path: &Path,
    carried: &[(String, String)],
    settings_lost: bool,
    trust: Trust,
    earlier: Earlier,
) -> rusqlite::Result<(Connection, Opened)> {
    let mut fresh = Connection::open_in_memory()?;
    let turned_off = populate(&mut fresh, carried, settings_lost)?;
    // Inside the file the swap renames into place: the repair and the duty to tell of it are
    // one rename.
    let rebuilt = Opened::Rebuilt {
        turned_off: turned_off.clone(),
        trust,
    };
    record_notice(&fresh, &rebuilt, &earlier)?;
    let placed = replace(path, &fresh).and_then(|aside| {
        crate::verr!(
            "[SETTINGS] settings.db was damaged, a copy is kept as {}; {} settings carried over",
            aside.display(),
            carried.len()
        );
        Connection::open(path)
            .and_then(|mut conn| init_schema(&mut conn).map(|()| conn))
            .map_err(|e| format!("reopen: {e}"))
    });
    match placed {
        Ok(conn) => Ok((conn, Opened::Rebuilt { turned_off, trust })),
        Err(why) => {
            crate::verr!(
                "[SETTINGS] cannot rebuild settings.db ({why}), settings stay in memory this run"
            );
            // The row written for the file that was not placed says what this run cannot do.
            let volatile = Opened::Volatile { turned_off, trust };
            record_notice(&fresh, &volatile, &earlier)?;
            Ok((fresh, volatile))
        }
    }
}

/// `REINDEX` and the markers for what the table lost, in one transaction: a crash between the two
/// must not leave a repaired index with no trace of the loss. `None` when the index will not
/// rebuild, or the file is still not clean after it.
fn reindex(
    conn: &Connection,
    settings_lost: bool,
    earlier: &Earlier,
) -> rusqlite::Result<Option<Opened>> {
    let tx = conn.unchecked_transaction()?;
    let trust = trust_lost_to_reindex(&tx);
    if tx.execute_batch("REINDEX").is_err() || !is_clean(&tx).unwrap_or(false) {
        return Ok(None);
    }
    let turned_off = if settings_lost {
        mark_lost(&tx, |key| has_row(&tx, key))?
    } else {
        Vec::new()
    };
    let reindexed = Opened::Reindexed { turned_off, trust };
    record_notice(&tx, &reindexed, earlier)?;
    tx.commit()?;
    Ok(Some(reindexed))
}

/// Opens settings.db behind `PRAGMA integrity_check`. The check sees what a read cannot: an index
/// that lost a row answers `Ok(None)`, the same as a setting never written. Corruption
/// (`CORRUPT`, `NOTADB`) that `REINDEX` does not clear is rebuilt, a copy of the damaged file
/// kept beside it. Anything else (`BUSY`, `READONLY`, `IOERR`, `CANTOPEN`) returns with the file
/// untouched: a transient fault is no reason to discard what the user chose.
///
/// Settings rows count as possibly lost only when the `settings` table itself fails, has left the
/// schema, or did not hand every row to the salvage: damage confined to `native_trust` leaves
/// every setting as the user chose it.
pub(crate) fn open(path: &Path) -> rusqlite::Result<(Connection, Opened)> {
    let opened = Connection::open(path).and_then(|mut conn| {
        let vanished = Vanished {
            settings: table_vanished(&conn, &SETTINGS)?,
            trust: table_vanished(&conn, &NATIVE_TRUST)?,
            notice: table_vanished(&conn, &REPAIR_NOTICE)?,
        };
        // `init_schema` would put an empty `settings` in the place of the vanished one, and the
        // next launch would find nothing missing.
        if !vanished.settings {
            init_schema(&mut conn)?;
        }
        let clean = is_clean(&conn)?;
        Ok((conn, clean, vanished))
    });
    match opened {
        Ok((
            conn,
            true,
            Vanished {
                settings: false, ..
            },
        )) => Ok((conn, Opened::Healthy)),
        Ok((conn, _, vanished)) => {
            // Asked before REINDEX, which rebuilds an index from its table: a row the table lost
            // would leave the index too, and every check would pass.
            let settings_lost = !settings_clean(&conn);
            // Before REINDEX for the same reason: a row the trust table lost would leave its
            // index too. A table that left the schema was put back empty by `init_schema`: it
            // reads as holding nothing, which is not known.
            let trust = if vanished.trust {
                Trust::Unknown
            } else {
                trust_at_open(&conn)
            };
            // A repair the user was never told of stays part of what this one tells.
            let earlier = earlier_notice(&conn, vanished.notice);
            if !vanished.settings
                && let Some(reindexed) = reindex(&conn, settings_lost, &earlier)?
            {
                crate::verr!(
                    "[SETTINGS] an index of settings.db disagreed with its table, rebuilt"
                );
                return Ok((conn, reindexed));
            }
            let salvaged = salvage(&conn);
            drop(conn);
            rebuild(
                path,
                &salvaged.rows,
                settings_lost || !salvaged.intact,
                trust,
                earlier,
            )
        }
        // Nothing could be read without a connection, not even the earlier notice.
        Err(e) if corruption(&e) => rebuild(path, &[], true, Trust::Unknown, Earlier::Unreadable),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/settings/store_damage_tests.rs"]
mod store_damage_tests;
