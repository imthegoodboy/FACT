//! `fact wallpaper`: an image or a looping video behind the whole workspace.
//!
//! FACT paints the wallpaper itself, so it shows in every terminal. Images are
//! copied into FACT's folder; videos are played from where they are (through
//! ffmpeg). The choice is recorded in `wallpaper\settings.json`.
//!
//! For images FACT also writes a Windows Terminal *fragment*: a separate JSON
//! file adding a "FACT" profile whose own background image Windows Terminal
//! draws at full resolution, with FACT transparent on top. The user's
//! `settings.json` is never edited, and `fact wallpaper clear` removes it all.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Fixed so re-running `fact wallpaper set` updates the same profile.
pub const PROFILE_GUID: &str = "{4c8f3a1d-6b2e-4f9a-8d71-3e5c0b9a2f64}";
pub const PROFILE_NAME: &str = "FACT";
const FRAGMENT_FILE: &str = "fact.json";
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
const RECORD_FILE: &str = "settings.json";
pub const DEFAULT_OPACITY: f64 = 0.35;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImageFit {
    /// Cover the window, cropping the edges if needed.
    Fill,
    /// Show the whole image, letterboxed.
    Fit,
    /// Stretch to the window, ignoring the aspect ratio.
    Stretch,
    /// Original size, centered.
    Center,
}

impl ImageFit {
    fn stretch_mode(self) -> &'static str {
        match self {
            ImageFit::Fill => "uniformToFill",
            ImageFit::Fit => "uniform",
            ImageFit::Stretch => "fill",
            ImageFit::Center => "none",
        }
    }

    fn from_stretch_mode(mode: &str) -> Option<Self> {
        [
            ImageFit::Fill,
            ImageFit::Fit,
            ImageFit::Stretch,
            ImageFit::Center,
        ]
        .into_iter()
        .find(|fit| fit.stretch_mode() == mode)
    }

    fn label(self) -> &'static str {
        match self {
            ImageFit::Fill => "fill",
            ImageFit::Fit => "fit",
            ImageFit::Stretch => "stretch",
            ImageFit::Center => "center",
        }
    }
}

/// Accept `35`, `35%` or `0.35`. A `%` suffix always means percent.
pub fn parse_opacity(text: &str) -> Result<f64, String> {
    let trimmed = text.trim();
    let (number, percent) = match trimmed.strip_suffix('%') {
        Some(number) => (number, true),
        None => (trimmed, false),
    };
    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("'{text}' is not a number"))?;
    let value = if percent || value > 1.0 {
        value / 100.0
    } else {
        value
    };
    if !(0.05..=1.0).contains(&value) {
        return Err("opacity must be between 5% and 100%".into());
    }
    Ok(value)
}

pub struct Locations {
    /// FACT's own copy of the image lives here.
    pub image_dir: PathBuf,
    pub fragment: PathBuf,
}

impl Locations {
    pub fn resolve(base: &Path) -> Result<Self> {
        let fragments = match std::env::var_os("FACT_WT_FRAGMENTS_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .context("LOCALAPPDATA is not set, so Windows Terminal cannot be located")?
                .join("Microsoft")
                .join("Windows Terminal")
                .join("Fragments"),
        };
        Ok(Self {
            image_dir: base.join("wallpaper"),
            fragment: fragments.join("FACT").join(FRAGMENT_FILE),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Wallpaper {
    /// FACT's copy of an image, or the original video file.
    pub image: PathBuf,
    pub opacity: f64,
    pub fit: ImageFit,
    pub video: bool,
}

impl ImageFit {
    fn from_label(label: &str) -> Option<Self> {
        [
            ImageFit::Fill,
            ImageFit::Fit,
            ImageFit::Stretch,
            ImageFit::Center,
        ]
        .into_iter()
        .find(|fit| fit.label() == label)
    }
}

pub fn is_video(path: &Path) -> bool {
    fact_client::is_video_file(path)
}

/// The current wallpaper: FACT's own record, or (for setups made by 0.5.0)
/// the image named in FACT's Windows Terminal fragment.
pub fn current(locations: &Locations) -> Result<Option<Wallpaper>> {
    let record = locations.image_dir.join(RECORD_FILE);
    if record.is_file() {
        let text = fs::read_to_string(&record)
            .with_context(|| format!("could not read {}", record.display()))?;
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", record.display()))?;
        let Some(source) = value["source"].as_str() else {
            return Ok(None);
        };
        let image = PathBuf::from(source);
        return Ok(Some(Wallpaper {
            video: is_video(&image),
            image,
            opacity: value["opacity"].as_f64().unwrap_or(DEFAULT_OPACITY),
            fit: value["fit"]
                .as_str()
                .and_then(ImageFit::from_label)
                .unwrap_or(ImageFit::Fill),
        }));
    }
    if !locations.fragment.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&locations.fragment)
        .with_context(|| format!("could not read {}", locations.fragment.display()))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", locations.fragment.display()))?;
    let Some(profile) = value["profiles"]
        .as_array()
        .and_then(|profiles| profiles.iter().find(|p| p["guid"] == PROFILE_GUID))
    else {
        return Ok(None);
    };
    let Some(image) = profile["backgroundImage"].as_str() else {
        return Ok(None);
    };
    Ok(Some(Wallpaper {
        image: PathBuf::from(image),
        video: false,
        opacity: profile["backgroundImageOpacity"]
            .as_f64()
            .unwrap_or(DEFAULT_OPACITY),
        fit: profile["backgroundImageStretchMode"]
            .as_str()
            .and_then(ImageFit::from_stretch_mode)
            .unwrap_or(ImageFit::Fill),
    }))
}

/// Use an image (copied into FACT's folder) or a video (played in place).
pub fn set(locations: &Locations, source: &Path, opacity: f64, fit: ImageFit) -> Result<Wallpaper> {
    if !source.is_file() {
        bail!("file not found: {}", source.display());
    }
    let extension = source
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let video = is_video(source);
    if !video && !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        bail!(
            "unsupported file type '.{extension}' (images: {}; videos: {})",
            IMAGE_EXTENSIONS.join(", "),
            fact_client::VIDEO_EXTENSIONS.join(", ")
        );
    }
    if video && fact_client::find_ffmpeg().is_none() {
        bail!("video wallpapers need ffmpeg: install it with `winget install ffmpeg`, then run this again");
    }

    fs::create_dir_all(&locations.image_dir)
        .with_context(|| format!("could not create {}", locations.image_dir.display()))?;
    let image = if video {
        // Videos can be large; play the original instead of copying it.
        fs::canonicalize(source)
            .map(|path| strip_verbatim(&path))
            .unwrap_or_else(|_| source.to_path_buf())
    } else {
        // A fresh name per image: the installed profile keeps pointing at the
        // old copy until the new fragment has been written in full.
        copy_to_fresh_name(source, &locations.image_dir, &extension)?
    };
    let wallpaper = Wallpaper {
        image,
        opacity,
        fit,
        video,
    };
    // The record is FACT's source of truth, so it is written first; the
    // Windows Terminal profile follows it.
    let committed = write_record(locations, &wallpaper).and_then(|()| {
        write_fragment(
            locations,
            (!video).then_some(wallpaper.image.as_path()),
            opacity,
            fit,
        )
    });
    if let Err(error) = committed {
        if !video {
            let _ = fs::remove_file(&wallpaper.image);
        }
        return Err(error);
    }
    // The new wallpaper is committed. Old copies are only clutter, so a
    // failure to delete one must not turn this success into an error.
    let _ = remove_images(&locations.image_dir, Some(wallpaper.image.as_path()));
    Ok(wallpaper)
}

/// `canonicalize` returns `\\?\C:\...`; keep the familiar form.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.display().to_string();
    // `\\?\UNC\server\share\...` is the verbatim form of `\\server\share\...`.
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{share}"));
    }
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

fn write_record(locations: &Locations, wallpaper: &Wallpaper) -> Result<()> {
    let record = locations.image_dir.join(RECORD_FILE);
    let value = json!({
        "source": wallpaper.image.display().to_string(),
        "opacity": (wallpaper.opacity * 100.0).round() / 100.0,
        "fit": wallpaper.fit.label(),
    });
    let temp = record.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_string_pretty(&value)? + "\n")
        .with_context(|| format!("could not write {}", temp.display()))?;
    fs::rename(&temp, &record).with_context(|| format!("could not write {}", record.display()))
}

/// Copy `source` to a `wallpaper-<number>.<ext>` name that does not exist yet.
/// `create_new` guarantees the copy can never overwrite the image the
/// installed profile is using, even for two sets in the same instant.
fn copy_to_fresh_name(source: &Path, dir: &Path, extension: &str) -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let mut input =
        fs::File::open(source).with_context(|| format!("could not open {}", source.display()))?;
    for attempt in 0..1000u128 {
        let image = dir.join(format!("wallpaper-{}.{extension}", stamp + attempt));
        let mut output = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&image)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("could not create {}", image.display()))
            }
        };
        if let Err(error) = std::io::copy(&mut input, &mut output) {
            drop(output);
            let _ = fs::remove_file(&image);
            return Err(error).with_context(|| format!("could not copy {}", source.display()));
        }
        return Ok(image);
    }
    bail!("could not find a free file name in {}", dir.display())
}

/// FACT's own image names: `wallpaper-<digits>.<ext>`, plus the fixed
/// `wallpaper.<ext>` and `incoming.<ext>` names used before.
fn is_fact_image(path: &Path) -> bool {
    // Only image copies: a video is always the user's own file. TIFF copies
    // made by FACT 0.5 are still FACT's to clean up.
    let image = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| {
            IMAGE_EXTENSIONS.contains(&ext.as_str()) || ext == "tif" || ext == "tiff"
        });
    let Some(stem) = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|_| image)
    else {
        return false;
    };
    let stem = stem.to_ascii_lowercase();
    stem == "wallpaper"
        || stem == "incoming"
        || stem
            .strip_prefix("wallpaper-")
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// The "FACT" Windows Terminal profile. With an image, Windows Terminal draws
/// it at full resolution and FACT starts transparent; a video is drawn by
/// FACT, so the profile then only launches FACT.
fn write_fragment(
    locations: &Locations,
    image: Option<&Path>,
    opacity: f64,
    fit: ImageFit,
) -> Result<()> {
    let exe = std::env::current_exe().context("could not locate fact.exe")?;
    let mut profile = json!({
        "guid": PROFILE_GUID,
        "name": PROFILE_NAME,
        "commandline": format!("\"{}\"", exe.display()),
        "startingDirectory": "%USERPROFILE%",
        "background": "#000000",
    });
    if let Some(image) = image {
        profile["commandline"] = json!(format!("\"{}\" --transparent", exe.display()));
        profile["backgroundImage"] = json!(image.display().to_string());
        profile["backgroundImageOpacity"] = json!((opacity * 100.0).round() / 100.0);
        profile["backgroundImageStretchMode"] = json!(fit.stretch_mode());
        profile["backgroundImageAlignment"] = json!("center");
    }
    let fragment = json!({ "profiles": [profile] });
    if let Some(parent) = locations.fragment.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let temp = locations.fragment.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_string_pretty(&fragment)? + "\n")
        .with_context(|| format!("could not write {}", temp.display()))?;
    if let Err(error) = fs::rename(&temp, &locations.fragment) {
        let _ = fs::remove_file(&temp);
        return Err(error)
            .with_context(|| format!("could not write {}", locations.fragment.display()));
    }
    Ok(())
}

/// Remove FACT's fragment and image copy. Returns whether anything existed.
pub fn clear(locations: &Locations) -> Result<bool> {
    let mut removed = false;
    if locations.fragment.is_file() {
        fs::remove_file(&locations.fragment)
            .with_context(|| format!("could not remove {}", locations.fragment.display()))?;
        removed = true;
        if let Some(dir) = locations.fragment.parent() {
            // Only succeeds when FACT's fragment folder is now empty.
            let _ = fs::remove_dir(dir);
        }
    }
    if locations.image_dir.is_dir() {
        let record = locations.image_dir.join(RECORD_FILE);
        if record.is_file() {
            fs::remove_file(&record)
                .with_context(|| format!("could not remove {}", record.display()))?;
            removed = true;
        }
        removed |= remove_images(&locations.image_dir, None)?;
        let _ = fs::remove_dir(&locations.image_dir);
    }
    Ok(removed)
}

/// Delete FACT's image copies (see [`is_fact_image`]) except `keep`. Other
/// files in the folder are left alone.
fn remove_images(dir: &Path, keep: Option<&Path>) -> Result<bool> {
    let mut removed = false;
    for entry in fs::read_dir(dir).with_context(|| format!("could not read {}", dir.display()))? {
        let path = entry?.path();
        if is_fact_image(&path) && path.is_file() && Some(path.as_path()) != keep {
            fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Windows Terminal reads fragments only when it loads its settings, and it
/// reloads them whenever `settings.json` changes. Bumping that file's
/// modification time (its content is never touched) applies a new or removed
/// wallpaper at once, without restarting Windows Terminal. Returns how many
/// installs were nudged. Skipped when `FACT_WT_FRAGMENTS_DIR` points elsewhere.
pub fn reload_windows_terminal() -> usize {
    if std::env::var_os("FACT_WT_FRAGMENTS_DIR").is_some() {
        return 0;
    }
    let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        return 0;
    };
    let candidates = [
        local.join(r"Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json"),
        local.join(
            r"Packages\Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe\LocalState\settings.json",
        ),
        local.join(
            r"Packages\Microsoft.WindowsTerminalCanary_8wekyb3d8bbwe\LocalState\settings.json",
        ),
        local.join(r"Microsoft\Windows Terminal\settings.json"),
    ];
    candidates
        .iter()
        .filter(|path| {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()))
                .is_ok()
        })
        .count()
}

/// True inside a Windows Terminal tab that uses FACT's wallpaper profile.
pub fn in_fact_profile() -> bool {
    fact_client::in_windows_terminal()
        && std::env::var("WT_PROFILE_ID").is_ok_and(|id| {
            id.trim_matches(|c| c == '{' || c == '}')
                .eq_ignore_ascii_case(PROFILE_GUID.trim_matches(|c| c == '{' || c == '}'))
        })
}

/// Arguments for `wt.exe` that open FACT in a new tab of the current window
/// with the wallpaper profile. `None` when an argument cannot be passed
/// safely (Windows Terminal treats `;` as a command separator).
pub fn new_tab_arguments(
    exe: &Path,
    dir: Option<&Path>,
    fact_args: &[OsString],
) -> Option<Vec<OsString>> {
    let mut args: Vec<OsString> = ["-w", "0", "new-tab", "-p", PROFILE_NAME]
        .into_iter()
        .map(OsString::from)
        .collect();
    if let Some(dir) = dir {
        let mut dir = dir.as_os_str().to_os_string();
        // A trailing backslash would escape the closing quote ("C:\").
        if dir.to_string_lossy().ends_with('\\') {
            dir.push(".");
        }
        args.push("-d".into());
        args.push(dir);
    }
    args.push("--".into());
    args.push(exe.as_os_str().to_os_string());
    args.push("--transparent".into());
    args.extend(fact_args.iter().cloned());
    if args.iter().any(|arg| arg.to_string_lossy().contains(';')) {
        return None;
    }
    Some(args)
}

pub fn describe(wallpaper: &Wallpaper) -> String {
    format!(
        "{} ({}opacity {:.0}%, {})",
        wallpaper.image.display(),
        if wallpaper.video { "video, " } else { "" },
        wallpaper.opacity * 100.0,
        wallpaper.fit.label()
    )
}

impl From<ImageFit> for fact_client::WallpaperFit {
    fn from(fit: ImageFit) -> Self {
        match fit {
            ImageFit::Fill => Self::Fill,
            ImageFit::Fit => Self::Fit,
            ImageFit::Center => Self::Center,
            ImageFit::Stretch => Self::Stretch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tab_arguments_keep_paths_intact() {
        let exe = Path::new(r"C:\Program Files\FACT\fact.exe");
        let args = new_tab_arguments(exe, Some(Path::new(r"C:\")), &["--grid".into(), "4".into()])
            .unwrap();
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&text[..5], ["-w", "0", "new-tab", "-p", "FACT"]);
        assert_eq!(text[5..7], ["-d".to_string(), r"C:\.".to_string()]);
        assert_eq!(text[7], "--");
        assert_eq!(text[8], exe.display().to_string());
        assert_eq!(text[9..], ["--transparent", "--grid", "4"]);
        assert!(new_tab_arguments(exe, Some(Path::new(r"C:\a;b")), &[]).is_none());
        assert!(
            is_fact_image(Path::new("wallpaper-1.tiff")),
            "old TIFF copies"
        );
    }

    #[test]
    fn verbatim_prefixes_are_removed_without_breaking_shares() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Videos\a.mp4")),
            PathBuf::from(r"C:\Videos\a.mp4")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\a.mp4")),
            PathBuf::from(r"\\server\share\a.mp4")
        );
        assert!(!is_fact_image(Path::new("wallpaper-123.mp4")));
    }

    #[test]
    fn opacity_accepts_percent_and_fraction() {
        assert_eq!(parse_opacity("35").unwrap(), 0.35);
        assert_eq!(parse_opacity("35%").unwrap(), 0.35);
        assert_eq!(parse_opacity("0.5").unwrap(), 0.5);
        assert_eq!(parse_opacity("100").unwrap(), 1.0);
        assert_eq!(parse_opacity("50%").unwrap(), 0.5);
        assert!(parse_opacity("1%").is_err(), "1% is below the 5% minimum");
        assert_eq!(parse_opacity("1").unwrap(), 1.0);
        assert!(parse_opacity("0").is_err());
        assert!(parse_opacity("250").is_err());
        assert!(parse_opacity("dim").is_err());
    }

    #[test]
    fn set_writes_a_profile_and_clear_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let locations = Locations {
            image_dir: dir.path().join("FACT").join("wallpaper"),
            fragment: dir.path().join("Fragments").join("FACT").join("fact.json"),
        };
        assert!(current(&locations).unwrap().is_none());
        let source = dir.path().join("Sunset.PNG");
        fs::write(&source, b"not really a png").unwrap();
        let wallpaper = set(&locations, &source, 0.4, ImageFit::Fit).unwrap();
        let name = wallpaper.image.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("wallpaper") && name.ends_with(".png"),
            "{name}"
        );
        assert!(wallpaper.image.is_file());

        let fragment: Value =
            serde_json::from_str(&fs::read_to_string(&locations.fragment).unwrap()).unwrap();
        let profile = &fragment["profiles"][0];
        assert_eq!(profile["name"], PROFILE_NAME);
        assert_eq!(profile["backgroundImageStretchMode"], "uniform");
        assert!(profile["commandline"]
            .as_str()
            .unwrap()
            .ends_with("--transparent"));
        assert_eq!(current(&locations).unwrap(), Some(wallpaper));
        assert!(locations.image_dir.join(RECORD_FILE).is_file());

        // Replacing the image with another type leaves exactly one copy.
        let jpeg = dir.path().join("photo.jpg");
        fs::write(&jpeg, b"jpeg").unwrap();
        set(&locations, &jpeg, DEFAULT_OPACITY, ImageFit::Fill).unwrap();
        let images = fs::read_dir(&locations.image_dir)
            .unwrap()
            .filter(|entry| is_fact_image(&entry.as_ref().unwrap().path()))
            .count();
        assert_eq!(images, 1, "one image copy next to the settings record");

        assert!(clear(&locations).unwrap());
        assert!(!locations.fragment.exists());
        assert!(!locations.image_dir.exists());
        assert!(!clear(&locations).unwrap());
    }

    #[test]
    fn only_fact_image_names_are_cleaned_up() {
        for ours in [
            "wallpaper.png",
            "incoming.jpg",
            "wallpaper-1730000000000.gif",
        ] {
            assert!(is_fact_image(Path::new(ours)), "{ours}");
        }
        for theirs in [
            "wallpaper-backup.png",
            "incoming-notes.txt",
            "sky.png",
            "wallpaper-.png",
        ] {
            assert!(!is_fact_image(Path::new(theirs)), "{theirs}");
        }
        let dir = tempfile::tempdir().unwrap();
        let locations = Locations {
            image_dir: dir.path().join("wallpaper"),
            fragment: dir.path().join("Fragments").join("FACT").join("fact.json"),
        };
        fs::create_dir_all(&locations.image_dir).unwrap();
        let keep = locations.image_dir.join("wallpaper-backup.png");
        fs::write(&keep, b"user file").unwrap();
        let source = dir.path().join("a.png");
        fs::write(&source, b"a").unwrap();
        let first = set(&locations, &source, 0.3, ImageFit::Fill).unwrap();
        let second = set(&locations, &source, 0.3, ImageFit::Fill).unwrap();
        assert_ne!(first.image, second.image, "each set gets a fresh name");
        assert!(!first.image.exists() && second.image.is_file());
        assert!(keep.is_file(), "unrelated files survive set");
        clear(&locations).unwrap();
        assert!(keep.is_file(), "unrelated files survive clear");
    }

    #[test]
    fn videos_play_in_place_without_a_terminal_image() {
        if fact_client::find_ffmpeg().is_none() {
            return; // set() refuses videos without ffmpeg; covered by its error.
        }
        let dir = tempfile::tempdir().unwrap();
        let locations = Locations {
            image_dir: dir.path().join("FACT").join("wallpaper"),
            fragment: dir.path().join("Fragments").join("FACT").join("fact.json"),
        };
        let video = dir.path().join("clip.mp4");
        fs::write(&video, b"not really a video").unwrap();
        let wallpaper = set(&locations, &video, 0.5, ImageFit::Fill).unwrap();
        assert!(wallpaper.video);
        assert_eq!(
            fs::canonicalize(&wallpaper.image).unwrap(),
            fs::canonicalize(&video).unwrap()
        );
        let fragment: Value =
            serde_json::from_str(&fs::read_to_string(&locations.fragment).unwrap()).unwrap();
        assert!(fragment["profiles"][0]["backgroundImage"].is_null());
        assert_eq!(current(&locations).unwrap(), Some(wallpaper));
        assert!(clear(&locations).unwrap());
        assert!(video.is_file(), "clear never deletes the user's video");
    }

    #[test]
    fn unsupported_or_missing_images_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let locations = Locations {
            image_dir: dir.path().join("wallpaper"),
            fragment: dir.path().join("fact.json"),
        };
        assert!(set(
            &locations,
            &dir.path().join("missing.png"),
            0.3,
            ImageFit::Fill
        )
        .is_err());
        let text = dir.path().join("notes.txt");
        fs::write(&text, b"hi").unwrap();
        assert!(set(&locations, &text, 0.3, ImageFit::Fill).is_err());
        assert!(!locations.fragment.exists());
    }
}
