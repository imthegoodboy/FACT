//! `fact wallpaper`: a background image behind the whole workspace.
//!
//! A console program cannot draw a real image, but Windows Terminal can. FACT
//! copies the image into its own directory and writes a Windows Terminal
//! *fragment*: a separate JSON file that adds a "FACT" profile with that
//! background image and starts `fact --transparent`. The user's own
//! `settings.json` is never edited, and `fact wallpaper clear` removes both
//! files again. In transparent mode FACT leaves default backgrounds to the host
//! terminal, so the image shows through every pane at zero runtime cost.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Fixed so re-running `fact wallpaper set` updates the same profile.
const PROFILE_GUID: &str = "{4c8f3a1d-6b2e-4f9a-8d71-3e5c0b9a2f64}";
pub const PROFILE_NAME: &str = "FACT";
const FRAGMENT_FILE: &str = "fact.json";
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"];
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
    pub image: PathBuf,
    pub opacity: f64,
    pub fit: ImageFit,
}

/// The wallpaper recorded in FACT's fragment, if one is installed.
pub fn current(locations: &Locations) -> Result<Option<Wallpaper>> {
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
        opacity: profile["backgroundImageOpacity"]
            .as_f64()
            .unwrap_or(DEFAULT_OPACITY),
        fit: profile["backgroundImageStretchMode"]
            .as_str()
            .and_then(ImageFit::from_stretch_mode)
            .unwrap_or(ImageFit::Fill),
    }))
}

/// Copy the image, then write the Windows Terminal profile that uses it.
pub fn set(locations: &Locations, source: &Path, opacity: f64, fit: ImageFit) -> Result<Wallpaper> {
    if !source.is_file() {
        bail!("image not found: {}", source.display());
    }
    let extension = source
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        bail!(
            "unsupported image type '.{extension}' (use {})",
            IMAGE_EXTENSIONS.join(", ")
        );
    }

    fs::create_dir_all(&locations.image_dir)
        .with_context(|| format!("could not create {}", locations.image_dir.display()))?;
    // A fresh name per image: the installed profile keeps pointing at the old
    // copy until the new fragment has been written in full.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let image = locations
        .image_dir
        .join(format!("wallpaper-{stamp}.{extension}"));
    if let Err(error) = fs::copy(source, &image) {
        let _ = fs::remove_file(&image);
        return Err(error).with_context(|| format!("could not copy {}", source.display()));
    }
    if let Err(error) = write_fragment(locations, &image, opacity, fit) {
        let _ = fs::remove_file(&image);
        return Err(error);
    }
    remove_images(&locations.image_dir, Some(&image))?;

    Ok(Wallpaper {
        image,
        opacity,
        fit,
    })
}

fn write_fragment(locations: &Locations, image: &Path, opacity: f64, fit: ImageFit) -> Result<()> {
    let exe = std::env::current_exe().context("could not locate fact.exe")?;
    let fragment = json!({
        "profiles": [{
            "guid": PROFILE_GUID,
            "name": PROFILE_NAME,
            "commandline": format!("\"{}\" --transparent", exe.display()),
            "startingDirectory": "%USERPROFILE%",
            "background": "#000000",
            "backgroundImage": image.display().to_string(),
            "backgroundImageOpacity": (opacity * 100.0).round() / 100.0,
            "backgroundImageStretchMode": fit.stretch_mode(),
            "backgroundImageAlignment": "center",
        }]
    });
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
        removed |= remove_images(&locations.image_dir, None)?;
        let _ = fs::remove_dir(&locations.image_dir);
    }
    Ok(removed)
}

/// Delete FACT's image copies (and staging files left by older versions),
/// except `keep`.
fn remove_images(dir: &Path, keep: Option<&Path>) -> Result<bool> {
    let mut removed = false;
    for entry in fs::read_dir(dir).with_context(|| format!("could not read {}", dir.display()))? {
        let path = entry?.path();
        let ours = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|stem| stem.starts_with("wallpaper") || stem.starts_with("incoming"));
        if ours && path.is_file() && Some(path.as_path()) != keep {
            fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
            removed = true;
        }
    }
    Ok(removed)
}

pub fn describe(wallpaper: &Wallpaper) -> String {
    format!(
        "{} (opacity {:.0}%, {})",
        wallpaper.image.display(),
        wallpaper.opacity * 100.0,
        wallpaper.fit.label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Replacing the image with another type leaves exactly one copy.
        let jpeg = dir.path().join("photo.jpg");
        fs::write(&jpeg, b"jpeg").unwrap();
        set(&locations, &jpeg, DEFAULT_OPACITY, ImageFit::Fill).unwrap();
        let images: Vec<_> = fs::read_dir(&locations.image_dir).unwrap().collect();
        assert_eq!(images.len(), 1);

        assert!(clear(&locations).unwrap());
        assert!(!locations.fragment.exists());
        assert!(!locations.image_dir.exists());
        assert!(!clear(&locations).unwrap());
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
