//! On-disk layout: `DATA_DIR/frames/YYYY-MM-DD/YYYYMMDD-HHMM.jpg`, all UTC.
//! Filenames are the only state. Anything not matching the patterns is ignored.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

const DAY_FORMAT: &str = "%Y-%m-%d";
const FRAME_FORMAT: &str = "%Y%m%d-%H%M";
const PART_SUFFIX: &str = ".part";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub name: String,
    pub time: DateTime<Utc>,
}

impl Frame {
    /// URL path of the raw file, served by `ServeDir` under `/frames`.
    pub fn url(&self) -> String {
        format!("/frames/{}/{}", self.time.format(DAY_FORMAT), self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaySummary {
    pub date: NaiveDate,
    pub frame_count: usize,
}

#[derive(Debug, Clone)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn frames_dir(&self) -> PathBuf {
        self.data_dir.join("frames")
    }

    pub fn day_dir(&self, date: NaiveDate) -> PathBuf {
        self.frames_dir().join(date.format(DAY_FORMAT).to_string())
    }

    pub fn frame_path(&self, time: DateTime<Utc>) -> PathBuf {
        self.day_dir(time.date_naive())
            .join(format!("{}.jpg", time.format(FRAME_FORMAT)))
    }

    /// Days that contain a valid day directory, newest first.
    pub fn list_days(&self) -> io::Result<Vec<DaySummary>> {
        let mut days = Vec::new();
        for date in self.day_dates()? {
            let frame_count = self.list_frames(date)?.len();
            days.push(DaySummary { date, frame_count });
        }
        days.reverse();
        Ok(days)
    }

    /// Valid frames for one day, oldest first. `NotFound` if the day directory is missing.
    pub fn list_frames(&self, date: NaiveDate) -> io::Result<Vec<Frame>> {
        let mut frames = Vec::new();
        for entry in fs::read_dir(self.day_dir(date))? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(time) = parse_frame_name(name) {
                frames.push(Frame {
                    name: name.to_owned(),
                    time,
                });
            }
        }
        frames.sort_by_key(|f| f.time);
        Ok(frames)
    }

    /// The newest valid frame, searching backwards through days until one has frames.
    pub fn newest_frame(&self) -> io::Result<Option<Frame>> {
        for date in self.day_dates()?.into_iter().rev() {
            if let Some(frame) = self.list_frames(date)?.pop() {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    /// Delete every `*.part` file under the data dir. Returns how many were removed.
    pub fn cleanup_parts(&self) -> io::Result<usize> {
        remove_parts(&self.data_dir)
    }

    /// Valid day directory dates, oldest first. Empty if the frames dir does not exist.
    fn day_dates(&self) -> io::Result<Vec<NaiveDate>> {
        let entries = match fs::read_dir(self.frames_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut dates = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(date) = entry.file_name().to_str().and_then(parse_day_name) {
                dates.push(date);
            }
        }
        dates.sort();
        Ok(dates)
    }
}

/// Path of the temporary file ffmpeg writes before it is renamed into place.
pub fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// Parse `YYYY-MM-DD`.
pub fn parse_day_name(name: &str) -> Option<NaiveDate> {
    if name.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(name, DAY_FORMAT).ok()
}

/// Parse `YYYYMMDD-HHMM.jpg` as a UTC time.
pub fn parse_frame_name(name: &str) -> Option<DateTime<Utc>> {
    let stem = name.strip_suffix(".jpg")?;
    let all_digits = stem.bytes().enumerate().all(|(i, b)| match i {
        8 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    if stem.len() != 13 || !all_digits {
        return None;
    }
    NaiveDateTime::parse_from_str(stem, FRAME_FORMAT)
        .ok()
        .map(|t| t.and_utc())
}

fn remove_parts(dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            removed += remove_parts(&path)?;
        } else if path.as_os_str().to_string_lossy().ends_with(PART_SUFFIX) {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn frame_path_round_trips_through_parse() {
        let store = Store::new("/data");
        let t = at(2026, 10, 11, 7, 5);
        let path = store.frame_path(t);
        assert_eq!(
            path,
            PathBuf::from("/data/frames/2026-10-11/20261011-0705.jpg")
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(parse_frame_name(name), Some(t));
    }

    #[test]
    fn parse_frame_name_rejects_junk() {
        for name in [
            "20261011-0705.jpg.part",
            "20261011-0705.png",
            "20261011-2460.jpg",
            "2026101-07055.jpg",
            "+2026101-0705.jpg",
            "notes.txt",
            ".jpg",
        ] {
            assert_eq!(parse_frame_name(name), None, "{name}");
        }
    }

    #[test]
    fn parse_day_name_rejects_junk() {
        assert_eq!(
            parse_day_name("2026-10-11"),
            NaiveDate::from_ymd_opt(2026, 10, 11)
        );
        assert_eq!(parse_day_name("2026-13-01"), None);
        assert_eq!(parse_day_name("lost+found"), None);
        assert_eq!(parse_day_name("2026-1-011"), None);
    }

    #[test]
    fn list_days_is_newest_first_with_counts_and_ignores_junk() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        touch(&store.frame_path(at(2026, 10, 10, 0, 0)));
        touch(&store.frame_path(at(2026, 10, 11, 0, 0)));
        touch(&store.frame_path(at(2026, 10, 11, 0, 1)));
        touch(
            &store
                .day_dir(NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .join("notes.txt"),
        );
        fs::create_dir_all(store.frames_dir().join("lost+found")).unwrap();

        let days = store.list_days().unwrap();
        assert_eq!(
            days,
            vec![
                DaySummary {
                    date: NaiveDate::from_ymd_opt(2026, 10, 11).unwrap(),
                    frame_count: 2
                },
                DaySummary {
                    date: NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
                    frame_count: 1
                },
            ]
        );
    }

    #[test]
    fn list_days_without_frames_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Store::new(dir.path()).list_days().unwrap(), vec![]);
    }

    #[test]
    fn list_frames_missing_day_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Store::new(dir.path())
            .list_frames(NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn newest_frame_skips_empty_newest_day() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        touch(&store.frame_path(at(2026, 10, 10, 23, 58)));
        touch(&store.frame_path(at(2026, 10, 10, 23, 59)));
        fs::create_dir_all(store.day_dir(NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())).unwrap();

        let newest = store.newest_frame().unwrap().unwrap();
        assert_eq!(newest.time, at(2026, 10, 10, 23, 59));
        assert_eq!(newest.url(), "/frames/2026-10-10/20261010-2359.jpg");
    }

    #[test]
    fn newest_frame_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Store::new(dir.path()).newest_frame().unwrap(), None);
    }

    #[test]
    fn cleanup_parts_removes_only_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let frame = store.frame_path(at(2026, 10, 11, 0, 0));
        touch(&frame);
        touch(&part_path(&store.frame_path(at(2026, 10, 11, 0, 1))));
        touch(&dir.path().join("out/x.mp4.part"));
        touch(&dir.path().join("keep.txt"));

        assert_eq!(store.cleanup_parts().unwrap(), 2);
        assert!(frame.exists());
        assert!(dir.path().join("keep.txt").exists());
        assert!(!dir.path().join("out/x.mp4.part").exists());
    }
}
