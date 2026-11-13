//! 1 ファイルの上限を超える動画の分割。
//!
//! TeraBox の無料アカウントは 1 ファイル 4GiB ちょうどまでしか受け付けない
//! （4GiB+1MiB の precreate が errno 58 になる。実測）。上限を超える動画は
//! ffmpeg で時間ごとに区切り、`名前-1.mp4`、`名前-2.mp4` … として送る。
//!
//! バイト単位で切ると 2 個目以降が再生できないため、ストリームコピー
//! （`-c copy`、再エンコードなし）でキーフレーム位置で区切る。どの部品も
//! 単体で再生できる。区切り位置はキーフレームに依存し、部品の大きさは
//! ばらつくので、作った後に大きさを確かめ、超えていれば短く区切り直す。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::which::which;

/// 無料アカウントの 1 ファイル上限（この値ちょうどは通る）。
pub const SINGLE_FILE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// 分割の対象にする拡張子（小文字）。
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mkv", "mov", "avi", "ts", "m2ts", "mts", "webm", "wmv", "flv",
];

/// 区切りの長さを決めるときの目標（上限に対する割合）。前から順に試す。
const FACTORS: &[f64] = &[0.92, 0.8, 0.65, 0.5];

/// 動画の拡張子か。
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 作業用フォルダ。消し忘れないよう、手放すときに中身ごと消す。
pub struct WorkDir {
    pub path: PathBuf,
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 作業用フォルダを作る。分割した部品は元と同じくらいの大きさになるので、
/// 元の動画と同じフォルダ（同じドライブ）に置く。作れなければ一時フォルダ。
pub fn make_work_dir(src: &Path) -> Result<WorkDir> {
    let tag = format!(".tbx-split-{}", std::process::id());
    let near = src.parent().map(|p| p.join(&tag));
    for dir in near.into_iter().chain(std::iter::once(std::env::temp_dir().join(&tag))) {
        if std::fs::create_dir_all(&dir).is_ok() {
            return Ok(WorkDir { path: dir });
        }
    }
    bail!("分割用の作業フォルダを作れませんでした");
}

/// `name` を `stem-番号.ext` の形にしたときの、ffmpeg 用の出力パターン。
fn output_pattern(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    // ffmpeg の連番指定は `%d`。名前に含まれる `%` は `%%` にしておく。
    dir.join(format!("{}-%d.{ext}", stem.replace('%', "%%")))
}

/// 動画の長さ（秒）。
fn duration_secs(ffprobe: &Path, src: &Path) -> Result<f64> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(src)
        .output()
        .context("ffprobe を起動できませんでした")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|d| *d > 0.0)
        .ok_or_else(|| anyhow!("動画の長さを読み取れませんでした（ffprobe: {}）", text.trim()))
}

/// 作業フォルダの部品を番号順に並べる。
fn list_parts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut parts: Vec<(u32, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let num = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit('-').next())
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(n) = num {
            parts.push((n, path));
        }
    }
    parts.sort_by_key(|(n, _)| *n);
    Ok(parts.into_iter().map(|(_, p)| p).collect())
}

fn clear_dir(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `src` を `limit` 以下の部品に分ける。`name` は送るときの名前（部品名の元）。
///
/// 戻り値は番号順の部品のパス。
pub fn split_video(src: &Path, name: &str, work: &Path, limit: u64) -> Result<Vec<PathBuf>> {
    let ffmpeg = which("ffmpeg").ok_or_else(|| {
        anyhow!("4GiB を超える動画を分けるには ffmpeg が必要です（PATH に見つかりませんでした）")
    })?;
    let ffprobe = which("ffprobe").ok_or_else(|| {
        anyhow!("4GiB を超える動画を分けるには ffprobe が必要です（PATH に見つかりませんでした）")
    })?;

    let size = std::fs::metadata(src)?.len();
    let duration = duration_secs(&ffprobe, src)?;
    let pattern = output_pattern(work, name);

    for factor in FACTORS {
        let seg = duration * (limit as f64 * factor) / size as f64;
        // まず全ストリーム（字幕・音声多重を含む）で試し、コンテナが受け付けない
        // ストリームがあって失敗したら映像と音声だけでやり直す。
        let maps: [&[&str]; 2] = [&["-map", "0", "-dn", "-ignore_unknown"], &["-map", "0:v", "-map", "0:a?"]];
        let mut made = false;
        let mut last_err = String::new();
        for map in maps {
            clear_dir(work);
            let out = Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
                .arg(src)
                .args(map)
                .args(["-c", "copy", "-f", "segment", "-segment_time"])
                .arg(format!("{seg:.3}"))
                .args([
                    "-reset_timestamps", "1",
                    "-avoid_negative_ts", "make_zero",
                    "-segment_start_number", "1",
                ])
                .arg(&pattern)
                .output()
                .context("ffmpeg を起動できませんでした")?;
            if out.status.success() {
                made = true;
                break;
            }
            last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        }
        if !made {
            bail!("ffmpeg で分割できませんでした: {last_err}");
        }

        let parts = list_parts(work)?;
        if parts.is_empty() {
            bail!("ffmpeg が部品を作りませんでした");
        }
        let too_big = parts
            .iter()
            .any(|p| std::fs::metadata(p).map(|m| m.len() > limit).unwrap_or(true));
        if !too_big {
            return Ok(parts);
        }
        // キーフレームの間隔や区間ごとのビットレートの差で上限を超えた。短く区切り直す。
    }
    clear_dir(work);
    bail!("上限以下に分けられませんでした（キーフレームの間隔が長すぎる可能性があります）");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ext_is_case_insensitive() {
        assert!(is_video(Path::new("a/b.MP4")));
        assert!(is_video(Path::new("x.mkv")));
        assert!(!is_video(Path::new("x.zip")));
        assert!(!is_video(Path::new("noext")));
    }

    #[test]
    fn pattern_escapes_percent_and_keeps_ext() {
        let p = output_pattern(Path::new("w"), "100%_clip.mkv");
        assert!(p.ends_with("100%%_clip-%d.mkv"));
    }
}
