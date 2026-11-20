//! The database as a serde record: a version, then the rows.
//!
//! Deserializing reads a private [`Raw`] and converts it with `TryFrom`, so a
//! database that is not valid never exists: a record of another version, or an
//! old one with none, is [`InvalidDb::Version`], and a path listed twice is
//! [`InvalidDb::DuplicatePath`]. There is no migration; a rescan rebuilds the
//! table.
//!
//! A path that is not valid UTF-8 is stored as its bytes (unix), so a plugin
//! with such a name keeps its row, and its blacklist, across restarts. Where
//! that is not possible, serializing fails rather than dropping the row.

use super::os_path;
use super::table::{FileRow, PluginDb, RowChange};
use serde::de::{Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

/// The version of the record layout [`PluginDb`] serializes and deserializes.
pub const DB_VERSION: u64 = 7;

/// Why a record is not a database this version can read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidDb {
    /// Another version wrote it; `found` is `0` for a record with no version.
    #[error("the plugin database is version {found}, this reads version {DB_VERSION}")]
    Version {
        /// The version the record declares.
        found: u64,
    },
    /// A row holds a plugin id of another format than its own.
    #[error("the plugin database row of {path:?}: {mismatch}")]
    FormatMismatch {
        /// The row's path.
        path: PathBuf,
        /// What is wrong with it.
        mismatch: crate::FormatMismatch,
    },
    /// Two rows name one path, so the record does not say which one holds.
    #[error("the plugin database lists {0:?} twice")]
    DuplicatePath(PathBuf),
}

/// A path as the record holds it: text, or the raw bytes of a name that is not
/// UTF-8. Externally tagged, so a format with no self-description reads it.
#[derive(Serialize)]
enum StoredPath<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

#[derive(Deserialize)]
enum StoredPathOwned {
    Text(String),
    Bytes(Vec<u8>),
}

impl<'a> StoredPath<'a> {
    fn of(path: &'a Path) -> Option<Self> {
        match path.to_str() {
            Some(text) => Some(Self::Text(text)),
            None => os_path::raw_bytes(path).map(Self::Bytes),
        }
    }
}

impl StoredPathOwned {
    fn into_path(self) -> PathBuf {
        match self {
            Self::Text(text) => PathBuf::from(text),
            Self::Bytes(bytes) => os_path::from_bytes(&bytes),
        }
    }
}

#[derive(Serialize)]
struct Out<'a> {
    version: u64,
    files: Vec<(StoredPath<'a>, &'a FileRow)>,
}

/// The record as read, before it is checked.
///
/// The version is read first and the rows only when it is this one: a record of
/// another version, whatever its rows look like, is [`InvalidDb::Version`] and
/// never a decoding error. A refused record holds no rows.
pub(crate) struct Raw {
    version: u64,
    files: Vec<(StoredPathOwned, FileRow)>,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum Field {
    Version,
    Files,
}

struct RawVisitor;

impl<'de> Visitor<'de> for RawVisitor {
    type Value = Raw;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a plugin database record")
    }

    /// A format that writes a struct as a sequence (postcard): the version is
    /// the first element.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Raw, A::Error> {
        let version: u64 = seq
            .next_element()?
            .ok_or_else(|| A::Error::invalid_length(0, &self))?;
        if version != DB_VERSION {
            return Ok(Raw::refused(version));
        }
        let files = seq
            .next_element()?
            .ok_or_else(|| A::Error::invalid_length(1, &self))?;
        Ok(Raw { version, files })
    }

    /// A format that writes a struct as a map (JSON): the writer puts `version`
    /// first, so a record whose first key is another is one with no version.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
        let version = match map.next_key::<Field>()? {
            Some(Field::Version) => map.next_value::<u64>()?,
            Some(Field::Files) => {
                map.next_value::<IgnoredAny>()?;
                0
            }
            None => 0,
        };
        if version != DB_VERSION {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            return Ok(Raw::refused(version));
        }
        let mut files = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Version => return Err(A::Error::duplicate_field("version")),
                Field::Files if files.is_some() => {
                    return Err(A::Error::duplicate_field("files"));
                }
                Field::Files => files = Some(map.next_value()?),
            }
        }
        let files = files.ok_or_else(|| A::Error::missing_field("files"))?;
        Ok(Raw { version, files })
    }
}

impl Raw {
    fn refused(version: u64) -> Self {
        Self {
            version,
            files: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("PluginDb", &["version", "files"], RawVisitor)
    }
}

impl TryFrom<Raw> for PluginDb {
    type Error = InvalidDb;

    fn try_from(raw: Raw) -> Result<Self, InvalidDb> {
        if raw.version != DB_VERSION {
            return Err(InvalidDb::Version { found: raw.version });
        }
        let mut db = PluginDb::new();
        for (path, row) in raw.files {
            let path = path.into_path();
            if db.row(&path).is_some() {
                return Err(InvalidDb::DuplicatePath(path));
            }
            row.check_formats()
                .map_err(|mismatch| InvalidDb::FormatMismatch {
                    path: path.clone(),
                    mismatch,
                })?;
            db.apply(RowChange::put(path, row));
        }
        Ok(db)
    }
}

impl Serialize for PluginDb {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut files = Vec::with_capacity(self.len());
        for (path, row) in self.rows() {
            let stored = StoredPath::of(path).ok_or_else(|| {
                S::Error::custom(format!(
                    "the path {path:?} cannot be stored in the database"
                ))
            })?;
            files.push((stored, row));
        }
        Out {
            version: DB_VERSION,
            files,
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::FileFacts;
    use crate::table::FileState;
    use tutti_plugin_types::{
        Described, LatencyVerdict, MaxBlock, Measured, NotChecked, PROBE_FLOOR, PluginClass,
        PluginDescriptor, PluginFormat, PluginId, ProbeConfig, StateHash, TailVerdict, Uid,
        Verdict, Widths,
    };
    use tutti_units::{Db, SampleRate, Samples};

    fn row(name: &str) -> FileRow {
        let facts = FileFacts::Present {
            mtime: crate::mtime::Mtime::new(
                std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123),
            ),
        };
        FileRow::probed(PluginFormat::Vst3, &facts)
            .plugins(vec![Described::new(
                PluginId::Vst3(Uid::new(std::array::from_fn(|i| {
                    name.as_bytes().get(i).copied().unwrap_or(0)
                }))),
                PluginDescriptor::new(name, PluginClass::Unknown),
            )])
            .build()
            .expect("ids of the row's format")
    }

    fn db_of<S: AsRef<str>>(names: &[S]) -> PluginDb {
        let mut db = PluginDb::new();
        for name in names {
            let name = name.as_ref();
            db.apply(RowChange::put(format!("/plugins/{name}.vst3"), row(name)));
        }
        db
    }

    fn no_file() -> FileFacts {
        FileFacts::Missing
    }

    /// The record as text, `version` first as the writer emits it.
    fn to_json(db: &PluginDb) -> String {
        serde_json::to_string(db).unwrap()
    }

    fn from_json(text: &str) -> Result<PluginDb, serde_json::Error> {
        serde_json::from_str(text)
    }

    fn raw_of(text: &str) -> Raw {
        serde_json::from_str(text).unwrap()
    }

    /// The text of the current version's `version` entry.
    fn version_entry(version: u64) -> String {
        format!("\"version\":{version}")
    }

    /// A row of one plugin whose `measured` is `measured`.
    fn measured_row(name: &str, measured: Measured) -> FileRow {
        let mut row = row(name);
        let FileState::Probed { plugins, .. } = &mut row.state else {
            unreachable!("a probed row");
        };
        plugins[0].descriptor.measured = Some(measured);
        row
    }

    fn measurement(tail: TailVerdict, latency: LatencyVerdict) -> Measured {
        Measured {
            tail: Some(Samples::new(8191)),
            latency: Some(Samples::new(64)),
            config: ProbeConfig {
                rate: SampleRate::SR_44K1,
                block: MaxBlock::new(Samples::new(64)).unwrap(),
                floor: PROBE_FLOOR,
                state: StateHash::of(b"patch"),
            },
            verdict: Verdict { tail, latency },
        }
    }

    /// **A populated database survives a round trip**: probed rows, a
    /// blacklisted file with no time, and a measurement of every verdict a probe
    /// can write read back equal.
    #[test]
    fn a_populated_database_round_trips() {
        let mut db = db_of(&["reverb", "delay"]);
        db.blacklist(
            Path::new("/plugins/crashy.vst3"),
            "segfault".into(),
            &no_file(),
        );
        let held = Db::new(-60.0);
        let verdicts = [
            (TailVerdict::Honest, LatencyVerdict::Honest),
            (
                TailVerdict::TailLonger {
                    reported: Samples::new(8191),
                    measured: Samples::new(12_287),
                },
                LatencyVerdict::LatencyDiffers {
                    reported: Samples::new(0),
                    measured: Samples::new(64),
                },
            ),
            (
                TailVerdict::NeverSettles { level: held },
                LatencyVerdict::NeverSettles { level: held },
            ),
            (
                TailVerdict::NotChecked(NotChecked::UnboundedClaim),
                LatencyVerdict::NotChecked(NotChecked::Instrument),
            ),
        ];
        for (k, (tail, latency)) in verdicts.into_iter().enumerate() {
            let name = format!("plugin{k}");
            db.apply(RowChange::put(
                format!("/plugins/{name}.vst3"),
                measured_row(&name, measurement(tail, latency)),
            ));
        }

        let back = from_json(&to_json(&db)).unwrap();

        assert_eq!(back, db);
        assert_eq!(back.len(), 7);
        assert!(back.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert_eq!(
            back.row(Path::new("/plugins/crashy.vst3")).unwrap().mtime,
            None
        );
        assert_eq!(
            back.plugins()
                .filter(|p| p.descriptor.measured.is_some())
                .count(),
            4
        );
    }

    /// **A populated database survives a format with no self-description**:
    /// postcard reads back exactly what it wrote, a blacklisted row and a
    /// measured one included.
    #[test]
    fn a_populated_database_round_trips_through_postcard() {
        let mut db = db_of(&["reverb", "delay"]);
        db.blacklist(
            Path::new("/plugins/crashy.vst3"),
            "segfault".into(),
            &no_file(),
        );
        db.apply(RowChange::put(
            "/plugins/measured.vst3",
            measured_row(
                "measured",
                measurement(TailVerdict::Honest, LatencyVerdict::Honest),
            ),
        ));

        let bytes = postcard::to_stdvec(&db).unwrap();
        let back: PluginDb = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(back, db);
    }

    /// **A plugin's widths survive both formats, each of the three answers**:
    /// 64-bit offered, 32-bit only, and unread.
    #[test]
    fn a_plugins_widths_round_trip_through_json_and_postcard() {
        let mut db = PluginDb::new();
        for (name, widths) in [
            ("both", Some(Widths::BOTH)),
            ("narrow", Some(Widths::F32)),
            ("unread", None),
        ] {
            let mut row = row(name);
            let FileState::Probed { plugins, .. } = &mut row.state else {
                unreachable!("a probed row");
            };
            plugins[0].descriptor.widths = widths;
            db.apply(RowChange::put(format!("/plugins/{name}.vst3"), row));
        }
        let widths_of = |db: &PluginDb, name: &str| {
            db.plugins()
                .find(|p| p.descriptor.name == name)
                .unwrap_or_else(|| panic!("{name} is in the database"))
                .descriptor
                .widths
        };

        let json = from_json(&to_json(&db)).unwrap();
        let postcard: PluginDb = postcard::from_bytes(&postcard::to_stdvec(&db).unwrap()).unwrap();

        for back in [&json, &postcard] {
            assert_eq!(widths_of(back, "both"), Some(Widths::BOTH));
            assert_eq!(widths_of(back, "narrow"), Some(Widths::F32));
            assert_eq!(widths_of(back, "unread"), None);
        }
    }

    /// A serialized database declares the version it deserializes.
    #[test]
    fn a_serialized_database_declares_the_version_it_reads() {
        let value: serde_json::Value = serde_json::from_str(&to_json(&db_of(&["a"]))).unwrap();
        assert_eq!(value["version"], DB_VERSION);
    }

    /// Rows come out in path order, so the same table serializes to the same
    /// bytes whatever order it was built in.
    #[test]
    fn insertion_order_does_not_change_the_serialized_bytes() {
        let a = serde_json::to_vec(&db_of(&["x", "y", "z", "w", "u", "v"])).unwrap();
        let b = serde_json::to_vec(&db_of(&["v", "u", "w", "z", "y", "x"])).unwrap();
        assert_eq!(a, b);
    }

    /// A record of another version is refused with the named error, on either
    /// side of this version. Each body is otherwise a valid current-version
    /// body, so a reader that ignored the version would accept it.
    #[test]
    fn a_record_of_another_version_is_refused() {
        let current = to_json(&db_of(&["reverb"]));
        for found in [DB_VERSION - 1, DB_VERSION + 1, 41] {
            let text = current.replace(&version_entry(DB_VERSION), &version_entry(found));

            assert_eq!(
                PluginDb::try_from(raw_of(&text)),
                Err(InvalidDb::Version { found }),
                "version {found}"
            );
            let message = from_json(&text).unwrap_err().to_string();
            assert!(
                message.contains(&format!("version {found}")),
                "version {found}: {message}"
            );
        }
    }

    /// A record of an older layout, whose rows this version cannot read, is
    /// refused by its version and not by a decoding error: the rows are never
    /// read. The text is a version 4 record, with an untagged path and a
    /// format on the row.
    #[test]
    fn an_older_layout_is_refused_by_its_version_not_decoded() {
        let text = r#"{"version":4,"files":[["/plugins/a.vst3",{"format":"Vst3","mtime":1700000000,"state":{"Probed":[]}}]]}"#;

        assert_eq!(
            PluginDb::try_from(raw_of(text)),
            Err(InvalidDb::Version { found: 4 })
        );
        let message = from_json(text).unwrap_err().to_string();
        assert!(message.contains("version 4"), "{message}");
    }

    /// The same holds for a format that writes a struct as a sequence: the
    /// version is the first element, and bytes after an older one are not read.
    #[test]
    fn an_older_layout_is_refused_by_its_version_in_postcard() {
        let older = [4_u8, 1, 0xff, 0xff, 0xff];

        let raw: Raw = postcard::from_bytes(&older).unwrap();

        assert_eq!(
            PluginDb::try_from(raw),
            Err(InvalidDb::Version { found: 4 })
        );
        assert!(postcard::from_bytes::<PluginDb>(&older).is_err());
    }

    /// A record with no version is version `0`, not the current one.
    #[test]
    fn a_record_with_no_version_is_refused() {
        let current = to_json(&db_of(&["reverb"]));
        let text = current.replace(&format!("{},", version_entry(DB_VERSION)), "");
        assert!(!text.contains(&version_entry(DB_VERSION)));

        assert_eq!(
            PluginDb::try_from(raw_of(&text)),
            Err(InvalidDb::Version { found: 0 })
        );
        assert!(from_json(&text).is_err());
    }

    /// A path listed twice is refused, naming the path: the record does not say
    /// which row holds.
    #[test]
    fn a_path_listed_twice_is_refused() {
        let (first, second) = (row("first"), row("second"));
        let text = serde_json::json!({
            "files": [[{"Text": "/plugins/a.vst3"}, first], [{"Text": "/plugins/a.vst3"}, second]],
        })
        .to_string()
        .replacen('{', &format!("{{{},", version_entry(DB_VERSION)), 1);

        assert_eq!(
            PluginDb::try_from(raw_of(&text)),
            Err(InvalidDb::DuplicatePath("/plugins/a.vst3".into()))
        );
        assert!(
            from_json(&text)
                .unwrap_err()
                .to_string()
                .contains("/plugins/a.vst3")
        );
    }

    /// The current version with a body of the wrong shape does not deserialize.
    #[test]
    fn the_current_version_with_a_bad_body_is_refused() {
        let v = version_entry(DB_VERSION);
        for body in [
            format!("{{{v},\"files\":7}}"),
            format!("{{{v}}}"),
            format!("{{{v},\"files\":[[\"/p/a.vst3\",5]]}}"),
            "[]".to_string(),
        ] {
            assert!(from_json(&body).is_err(), "{body}");
        }
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// A name that is not UTF-8 keeps its row, blacklist included, through a
        /// round trip: a crashing plugin with such a name must not be probed
        /// again after a restart.
        #[test]
        fn a_non_utf8_path_round_trips() {
            use std::os::unix::ffi::OsStrExt;
            let mut db = db_of(&["kept"]);
            let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/plugins/caf\xe9.vst3"));
            db.blacklist(&odd, "boom".into(), &no_file());

            let back = from_json(&to_json(&db)).unwrap();

            assert_eq!(back, db);
            assert!(back.is_blacklisted(&odd));
        }

        /// The same holds in a format with no self-description.
        #[test]
        fn a_non_utf8_path_round_trips_through_postcard() {
            use std::os::unix::ffi::OsStrExt;
            let mut db = db_of(&["kept"]);
            let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/plugins/caf\xe9.vst3"));
            db.blacklist(&odd, "boom".into(), &no_file());

            let bytes = postcard::to_stdvec(&db).unwrap();
            let back: PluginDb = postcard::from_bytes(&bytes).unwrap();

            assert_eq!(back, db);
        }
    }

    /// **A record whose row holds an id of another format is refused at
    /// decode**, naming the row's path: the database never holds it.
    #[test]
    fn a_row_with_an_id_of_another_format_is_refused_at_decode() {
        let mut stray = row("a");
        let FileState::Probed { plugins, .. } = &mut stray.state else {
            unreachable!("a probed row")
        };
        plugins[0].id = Some(PluginId::Vst2(7));
        let mut db = PluginDb::new();
        db.apply(RowChange::put("/p/a.vst3", stray));
        let written = serde_json::to_string(&db).unwrap();
        match serde_json::from_str::<PluginDb>(&written) {
            Err(error) => assert!(error.to_string().contains("vst2.7"), "{error}"),
            Ok(_) => panic!("a mixed-format row was read"),
        }
    }
}
