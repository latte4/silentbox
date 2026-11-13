//! フォルダ一括アップロードの走査。
//!
//! ローカルのフォルダを辿り、リモートに作るフォルダと送るファイルを決める。
//! 転送そのものは呼び出し側（CLI）が行う。ここではネットワークに触らない。
//!
//! `C:\写真` を `/保存先` へ送ると `/保存先/写真/…` になる（エクスプローラーで
//! フォルダを放り込んだときと同じ）。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// 送る 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    pub local: PathBuf,
    /// 置き場所（リモートの絶対パス）。
    pub remote_dir: String,
    pub name: String,
    pub size: u64,
    /// 表示用（送るフォルダからの相対パス）。
    pub display: String,
}

/// 走査結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalPlan {
    /// 作るフォルダ（リモートの絶対パス）。親が先に来るよう浅い順。
    pub dirs: Vec<String>,
    pub files: Vec<LocalFile>,
}

/// Windows が勝手に作る、送らなくてよいファイルか。
fn is_junk(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "desktop.ini" || lower == "thumbs.db" || lower.starts_with(".tbx-split-")
}

fn join_remote(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

/// `local_root` を `remote_parent` の下へ送る計画を立てる。
///
/// シンボリックリンク（ジャンクション含む）は辿らない。ループや、意図しない
/// 場所の中身まで送るのを避けるため。
pub fn plan(local_root: &Path, remote_parent: &str) -> std::io::Result<LocalPlan> {
    let root_remote = match local_root.file_name() {
        Some(name) => join_remote(remote_parent, &name.to_string_lossy()),
        // `C:\` のように名前が無いときは、保存先の直下へ中身を置く。
        None => remote_parent.to_string(),
    };

    let mut plan = LocalPlan::default();
    plan.dirs.push(root_remote.clone());

    // 幅優先で辿ると、作るフォルダが自然に浅い順になる。
    let mut queue: VecDeque<(PathBuf, String, String)> = VecDeque::new();
    queue.push_back((local_root.to_path_buf(), root_remote, String::new()));

    while let Some((dir, remote_dir, rel)) = queue.pop_front() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_junk(&name) {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                continue;
            }
            let rel_child = if rel.is_empty() { name.clone() } else { format!("{rel}\\{name}") };
            if ty.is_dir() {
                let child_remote = join_remote(&remote_dir, &name);
                plan.dirs.push(child_remote.clone());
                queue.push_back((entry.path(), child_remote, rel_child));
            } else if ty.is_file() {
                plan.files.push(LocalFile {
                    local: entry.path(),
                    remote_dir: remote_dir.clone(),
                    name,
                    size: entry.metadata()?.len(),
                    display: rel_child,
                });
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_nested_folders_under_root_name() {
        let tmp = std::env::temp_dir().join(format!("tbx-putdir-test-{}", std::process::id()));
        let root = tmp.join("写真");
        std::fs::create_dir_all(root.join("旅行").join("空")).unwrap();
        std::fs::write(root.join("a.jpg"), b"aa").unwrap();
        std::fs::write(root.join("旅行").join("b.mp4"), b"bbb").unwrap();
        std::fs::write(root.join("Thumbs.db"), b"x").unwrap();
        std::fs::write(root.join("desktop.ini"), b"x").unwrap();

        let p = plan(&root, "/保存先").unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();

        assert_eq!(p.dirs, vec!["/保存先/写真", "/保存先/写真/旅行", "/保存先/写真/旅行/空"]);
        let files: Vec<_> = p
            .files
            .iter()
            .map(|f| (f.remote_dir.as_str(), f.name.as_str(), f.size, f.display.as_str()))
            .collect();
        assert_eq!(
            files,
            vec![
                ("/保存先/写真", "a.jpg", 2, "a.jpg"),
                ("/保存先/写真/旅行", "b.mp4", 3, "旅行\\b.mp4"),
            ]
        );
    }

    #[test]
    fn root_destination_has_no_double_slash() {
        assert_eq!(join_remote("/", "x"), "/x");
        assert_eq!(join_remote("/a/", "x"), "/a/x");
    }
}
