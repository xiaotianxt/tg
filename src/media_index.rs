use md5::{Digest, Md5};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::parallel;

pub(crate) struct MediaIndex {
    categories: HashMap<String, CategoryIndex>,
}

#[derive(Debug, Clone)]
struct MediaDirSpec {
    path: PathBuf,
    category: String,
}

#[derive(Debug)]
struct IndexedMediaFile {
    path: PathBuf,
    lower_name: String,
    lower_stem: String,
    len: u64,
    is_thumb: bool,
}

struct CategoryIndex {
    files: Vec<IndexedMediaFile>,
    exact_names: HashMap<String, usize>,
    exact_stems: HashMap<String, usize>,
    by_name: Vec<usize>,
}

impl MediaIndex {
    pub(crate) fn load(
        base_path: &Path,
        session_id: &str,
        categories: &[&str],
        jobs: usize,
    ) -> Self {
        let scan_jobs = discover_media_dirs(base_path, session_id, categories);
        let scan_job_count = parallel::job_count(jobs, 8);
        let scanned = parallel::map_ordered(scan_jobs, scan_job_count, scan_media_dir);

        let mut files_by_category: HashMap<String, Vec<IndexedMediaFile>> = HashMap::new();
        for (category, files) in scanned {
            files_by_category.entry(category).or_default().extend(files);
        }

        Self {
            categories: files_by_category
                .into_iter()
                .map(|(category, files)| (category, CategoryIndex::new(files)))
                .collect(),
        }
    }

    pub(crate) fn find(&self, category: &str, identifier: &str) -> Option<PathBuf> {
        self.categories.get(category)?.find(identifier)
    }
}

impl CategoryIndex {
    fn new(files: Vec<IndexedMediaFile>) -> Self {
        let mut exact_names = HashMap::new();
        let mut exact_stems = HashMap::new();
        for index in 0..files.len() {
            insert_best(
                &mut exact_names,
                &files,
                files[index].lower_name.clone(),
                index,
            );
            insert_best(
                &mut exact_stems,
                &files,
                files[index].lower_stem.clone(),
                index,
            );
        }

        let mut by_name = (0..files.len()).collect::<Vec<_>>();
        by_name.sort_by(|&a, &b| files[a].lower_name.cmp(&files[b].lower_name));

        Self {
            files,
            exact_names,
            exact_stems,
            by_name,
        }
    }

    fn find(&self, identifier: &str) -> Option<PathBuf> {
        let lower = identifier.trim().to_lowercase();
        if lower.is_empty() {
            return None;
        }
        let lower_stem = Path::new(&lower)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("");

        self.find_exact(&lower)
            .or_else(|| {
                (lower_stem != lower && !lower_stem.is_empty())
                    .then(|| self.find_exact(lower_stem))
                    .flatten()
            })
            .or_else(|| self.find_prefix(&lower))
            .or_else(|| self.find_contains(&lower))
    }

    fn find_exact(&self, needle: &str) -> Option<PathBuf> {
        self.best_path(
            [
                self.exact_names.get(needle).copied(),
                self.exact_stems.get(needle).copied(),
            ]
            .into_iter()
            .flatten(),
        )
    }

    fn find_prefix(&self, needle: &str) -> Option<PathBuf> {
        let start = self
            .by_name
            .partition_point(|&index| self.files[index].lower_name.as_str() < needle);
        self.best_path(
            self.by_name[start..]
                .iter()
                .copied()
                .take_while(|&index| self.files[index].lower_name.starts_with(needle)),
        )
    }

    fn find_contains(&self, needle: &str) -> Option<PathBuf> {
        self.best_path(
            self.files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| file.lower_name.contains(needle).then_some(index)),
        )
    }

    fn best_path(&self, indices: impl IntoIterator<Item = usize>) -> Option<PathBuf> {
        let index = indices.into_iter().reduce(|best, candidate| {
            if is_better(&self.files[candidate], &self.files[best]) {
                candidate
            } else {
                best
            }
        })?;
        self.files[index]
            .path
            .is_file()
            .then(|| self.files[index].path.clone())
    }
}

fn insert_best(
    map: &mut HashMap<String, usize>,
    files: &[IndexedMediaFile],
    key: String,
    candidate: usize,
) {
    match map.get_mut(&key) {
        Some(best) if is_better(&files[candidate], &files[*best]) => *best = candidate,
        Some(_) => {}
        None => {
            map.insert(key, candidate);
        }
    }
}

fn is_better(candidate: &IndexedMediaFile, current: &IndexedMediaFile) -> bool {
    (!candidate.is_thumb, candidate.len) > (!current.is_thumb, current.len)
}

fn discover_media_dirs(
    base_path: &Path,
    session_id: &str,
    categories: &[&str],
) -> Vec<MediaDirSpec> {
    let mut dirs = Vec::new();

    let msg_temp = base_path.join("Message/MessageTemp").join(session_id);
    for category in categories {
        let message_temp_dir = msg_temp.join(category);
        if message_temp_dir.is_dir() {
            dirs.push(MediaDirSpec {
                path: message_temp_dir,
                category: (*category).to_string(),
            });
        }
    }

    if categories.contains(&"Image") {
        let attach_dir = base_path.join("msg/attach");
        if attach_dir.is_dir() {
            discover_attach_image_dirs(&attach_dir, &mut dirs);
        }
    }

    if categories.contains(&"File") {
        let file_dir = base_path.join("msg/file");
        if file_dir.is_dir() {
            dirs.push(MediaDirSpec {
                path: file_dir,
                category: "File".to_string(),
            });
        }

        let attach_dir = base_path.join("msg/attach");
        if attach_dir.is_dir() {
            discover_attach_file_dirs(&attach_dir, session_id, &mut dirs);
        }
    }

    if categories.contains(&"Video") {
        let video_dir = base_path.join("msg/video");
        if video_dir.is_dir() {
            if let Ok(months) = fs::read_dir(video_dir) {
                for month in months.flatten() {
                    let path = month.path();
                    if path.is_dir() {
                        dirs.push(MediaDirSpec {
                            path,
                            category: "Video".to_string(),
                        });
                    }
                }
            }
        }
    }

    dirs.sort_by(|a, b| a.path.cmp(&b.path).then(a.category.cmp(&b.category)));
    dirs.dedup_by(|a, b| a.path == b.path && a.category == b.category);
    dirs
}

fn discover_attach_image_dirs(attach_dir: &Path, dirs: &mut Vec<MediaDirSpec>) {
    let Ok(accounts) = fs::read_dir(attach_dir) else {
        return;
    };

    for account in accounts.flatten() {
        let account_path = account.path();
        if !account_path.is_dir() {
            continue;
        }
        let Ok(months) = fs::read_dir(account_path) else {
            continue;
        };
        for month in months.flatten() {
            let img_dir = month.path().join("Img");
            if img_dir.is_dir() {
                dirs.push(MediaDirSpec {
                    path: img_dir,
                    category: "Image".to_string(),
                });
            }
        }
    }
}

fn discover_attach_file_dirs(attach_dir: &Path, session_id: &str, dirs: &mut Vec<MediaDirSpec>) {
    fn walk(dir: &Path, dirs: &mut Vec<MediaDirSpec>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) == Some("F") {
                dirs.push(MediaDirSpec {
                    path,
                    category: "File".to_string(),
                });
            } else {
                walk(&path, dirs);
            }
        }
    }

    let session_dir = attach_dir.join(session_attach_dir_name(session_id));
    if session_dir.is_dir() {
        walk(&session_dir, dirs);
    }
}

fn session_attach_dir_name(session_id: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(session_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn scan_media_dir(spec: MediaDirSpec) -> (String, Vec<IndexedMediaFile>) {
    let mut files = Vec::new();
    scan_files(&spec.path, &mut files);
    (spec.category, files)
}

fn scan_files(dir: &Path, files: &mut Vec<IndexedMediaFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            scan_files(&path, files);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let lower_name = entry.file_name().to_string_lossy().to_lowercase();
        let lower_stem = Path::new(&lower_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        files.push(IndexedMediaFile {
            path,
            is_thumb: is_thumb_name(&lower_name),
            lower_name,
            lower_stem,
            len,
        });
    }
}

fn is_thumb_name(lower_name: &str) -> bool {
    lower_name.contains(".pic_thm") || lower_name.contains("thumb") || lower_name.contains("_thumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_and_finds_cached_image() {
        let temp = tempfile::tempdir().unwrap();
        let img_dir = temp.path().join("msg/attach/account/2026-04/Img");
        fs::create_dir_all(&img_dir).unwrap();
        fs::write(img_dir.join("abc_thumb.dat"), b"larger thumb").unwrap();
        fs::write(img_dir.join("abc.dat"), b"full-size").unwrap();

        let index = MediaIndex::load(temp.path(), "session", &["Image"], 1);

        assert_eq!(index.find("Image", "abc").unwrap(), img_dir.join("abc.dat"));
    }

    #[test]
    fn discovers_telegram4_image_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let img_dir = temp.path().join("msg/attach/account/2026-04/Img");
        fs::create_dir_all(&img_dir).unwrap();

        let dirs = discover_media_dirs(temp.path(), "session", &["Image"]);

        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, img_dir);
        assert_eq!(dirs[0].category, "Image");
    }

    #[test]
    fn discovers_telegram4_file_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = session_attach_dir_name("session");
        let file_dir = temp
            .path()
            .join("msg/attach")
            .join(session_dir)
            .join("2026-04/Rec/item/F");
        fs::create_dir_all(&file_dir).unwrap();
        fs::create_dir_all(temp.path().join("msg/attach/other/2026-04/Rec/item/F")).unwrap();

        let dirs = discover_media_dirs(temp.path(), "session", &["File"]);

        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, file_dir);
        assert_eq!(dirs[0].category, "File");
    }

    #[test]
    fn indexes_and_finds_cached_file() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = session_attach_dir_name("session");
        let file_dir = temp
            .path()
            .join("msg/attach")
            .join(session_dir)
            .join("2026-04/Rec/item/F");
        fs::create_dir_all(&file_dir).unwrap();
        fs::write(file_dir.join("report.pdf"), b"pdf").unwrap();

        let index = MediaIndex::load(temp.path(), "session", &["File"], 1);

        assert_eq!(
            index.find("File", "report.pdf").unwrap(),
            file_dir.join("report.pdf")
        );
    }

    #[test]
    fn indexes_and_finds_msg_file_cached_file() {
        let temp = tempfile::tempdir().unwrap();
        let file_dir = temp.path().join("msg/file/2026-04");
        fs::create_dir_all(&file_dir).unwrap();
        fs::write(file_dir.join("report.pdf"), b"pdf").unwrap();

        let index = MediaIndex::load(temp.path(), "session", &["File"], 1);

        assert_eq!(
            index.find("File", "report.pdf").unwrap(),
            file_dir.join("report.pdf")
        );
    }

    #[test]
    fn finds_cached_file_by_identifier_stem() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = session_attach_dir_name("session");
        let file_dir = temp
            .path()
            .join("msg/attach")
            .join(session_dir)
            .join("2026-04/Rec/item/F");
        fs::create_dir_all(&file_dir).unwrap();
        fs::write(file_dir.join("report"), b"pdf").unwrap();

        let index = MediaIndex::load(temp.path(), "session", &["File"], 1);

        assert_eq!(
            index.find("File", "report.pdf").unwrap(),
            file_dir.join("report")
        );
    }

    #[test]
    fn preserves_prefix_contains_and_quality_ordering() {
        let temp = tempfile::tempdir().unwrap();
        let img_dir = temp.path().join("msg/attach/account/2026-04/Img");
        fs::create_dir_all(&img_dir).unwrap();
        fs::write(img_dir.join("prefix_thumb.dat"), b"larger thumb").unwrap();
        fs::write(img_dir.join("prefix_full.dat"), b"full").unwrap();
        fs::write(img_dir.join("x%_needle_thumb.dat"), b"larger thumb").unwrap();
        fs::write(img_dir.join("y%_needle.dat"), b"full").unwrap();

        let index = MediaIndex::load(temp.path(), "session", &["Image"], 1);

        assert_eq!(
            index.find("Image", "prefix").unwrap(),
            img_dir.join("prefix_full.dat")
        );
        assert_eq!(
            index.find("Image", "%_needle").unwrap(),
            img_dir.join("y%_needle.dat")
        );
    }
}
