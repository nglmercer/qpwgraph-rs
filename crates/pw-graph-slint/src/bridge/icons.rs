//! Resolve backend-provided XDG icon names to images for graph nodes.
//!
//! PipeWire only gives us an icon name, not a ready-to-render Slint image.
//! Keep the theme lookup here, on the UI side, so changing themes or
//! packaging the application does not affect graph identity or routing.

use slint::Image;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Default)]
struct NodeIconCache {
    images: HashMap<String, Option<Image>>,
}

thread_local! {
    static NODE_ICON_CACHE: RefCell<NodeIconCache> = RefCell::new(NodeIconCache::default());
}

struct IconIndex {
    files: HashMap<String, Vec<PathBuf>>,
}

static ICON_INDEX: OnceLock<IconIndex> = OnceLock::new();

/// Load an optional XDG icon by name. A missing or invalid icon is treated as
/// normal: nodes without usable icon data simply render without an image.
pub(crate) fn load_node_icon(icon_name: Option<&str>) -> Option<Image> {
    let icon_name = icon_name?.trim();
    if icon_name.is_empty() {
        return None;
    }

    NODE_ICON_CACHE.with(|cache| {
        if let Some(image) = cache.borrow().images.get(icon_name).cloned() {
            return image;
        }
        let image = resolve_icon_path(icon_name).and_then(|path| Image::load_from_path(&path).ok());
        cache
            .borrow_mut()
            .images
            .insert(icon_name.to_owned(), image.clone());
        image
    })
}

fn resolve_icon_path(icon_name: &str) -> Option<PathBuf> {
    let direct = Path::new(icon_name);
    if direct.is_file() {
        return Some(direct.to_owned());
    }

    let index = ICON_INDEX.get_or_init(build_icon_index);
    let key = icon_name
        .rsplit_once('/')
        .map_or(icon_name, |(_, name)| name);
    let key = Path::new(key)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(key)
        .to_ascii_lowercase();
    let mut key = key;
    loop {
        if let Some(path) = index
            .files
            .get(&key)
            .and_then(|paths| paths.first())
            .or_else(|| {
                index
                    .files
                    .get(&format!("{key}-symbolic"))
                    .and_then(|paths| paths.first())
            })
        {
            return Some(path.clone());
        }
        let separator = key.rfind('-')?;
        key.truncate(separator);
    }
}

fn build_icon_index() -> IconIndex {
    let mut files = HashMap::<String, Vec<PathBuf>>::new();
    for root in icon_roots() {
        index_icon_directory(&root, 0, &mut files);
    }
    let preferred_theme = current_icon_theme();
    for paths in files.values_mut() {
        paths.sort_by_key(|path| icon_path_score(path, preferred_theme.as_deref()));
        paths.dedup();
    }
    IconIndex { files }
}

fn icon_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    if let Some(data_home) = data_home {
        roots.push(data_home.join("icons"));
        roots.push(data_home.join("pixmaps"));
    }

    let data_dirs = env::var_os("XDG_DATA_DIRS")
        .map(|dirs| env::split_paths(&dirs).collect::<Vec<_>>())
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| {
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        });
    for data_dir in data_dirs {
        roots.push(data_dir.join("icons"));
        roots.push(data_dir.join("pixmaps"));
    }
    if let Some(home) = env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".icons"));
    }
    roots.push(PathBuf::from("/usr/share/pixmaps"));
    roots
}

fn index_icon_directory(directory: &Path, depth: usize, files: &mut HashMap<String, Vec<PathBuf>>) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            index_icon_directory(&path, depth + 1, files);
            continue;
        }
        if !file_type.is_file() || !supported_icon_extension(&path) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        files
            .entry(stem.to_ascii_lowercase())
            .or_default()
            .push(path);
    }
}

fn supported_icon_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "svg" | "png" | "jpg" | "jpeg"
            )
        })
}

fn current_icon_theme() -> Option<String> {
    if let Some(theme) = env::var_os("XDG_ICON_THEME").and_then(|theme| theme.into_string().ok()) {
        let theme = theme.trim().to_owned();
        if !theme.is_empty() {
            return Some(theme);
        }
    }

    let home = env::var_os("HOME").map(PathBuf::from)?;
    let settings = [
        home.join(".config/gtk-4.0/settings.ini"),
        home.join(".config/gtk-3.0/settings.ini"),
        home.join(".config/kdeglobals"),
    ];
    for path in settings {
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let mut in_icons_section = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_icons_section = line.eq_ignore_ascii_case("[icons]");
            }
            let key = if in_icons_section {
                "Name"
            } else {
                "gtk-icon-theme-name"
            };
            let Some((candidate_key, value)) = line.split_once('=') else {
                continue;
            };
            if candidate_key.trim().eq_ignore_ascii_case(key) {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

fn icon_path_score(path: &Path, preferred_theme: Option<&str>) -> (u8, u8, u32, u8, String) {
    let theme_rank = path
        .components()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|components| {
            (components[0].as_os_str() == "icons")
                .then(|| components[1].as_os_str().to_string_lossy().into_owned())
        })
        .map_or(2, |theme| {
            if preferred_theme == Some(theme.as_str()) {
                0
            } else if theme == "hicolor" {
                1
            } else {
                2
            }
        });
    let size_rank = path
        .components()
        .find_map(|component| parse_icon_size(component.as_os_str().to_string_lossy().as_ref()));
    let (scalable_rank, size_distance, size) = match size_rank {
        Some(0) => (0, 0, 0),
        Some(size) => (1, size.abs_diff(32), size),
        None => (2, u32::MAX, u32::MAX),
    };
    let format_rank = match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("svg") => 0,
        Some(extension) if extension.eq_ignore_ascii_case("png") => 1,
        _ => 2,
    };
    (
        theme_rank,
        scalable_rank,
        size_distance.min(size),
        format_rank,
        path.to_string_lossy().into_owned(),
    )
}

fn parse_icon_size(component: &str) -> Option<u32> {
    if component.eq_ignore_ascii_case("scalable") {
        return Some(0);
    }
    let (width, height) = component.split_once('x')?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    (width == height).then_some(width)
}

#[cfg(test)]
mod tests {
    use super::{icon_path_score, parse_icon_size, resolve_icon_path};
    use slint::Image;
    use std::path::Path;

    #[test]
    fn parses_scalable_and_square_icon_sizes() {
        assert_eq!(parse_icon_size("scalable"), Some(0));
        assert_eq!(parse_icon_size("48x48"), Some(48));
        assert_eq!(parse_icon_size("48x32"), None);
    }

    #[test]
    fn prefers_the_current_theme_and_scalable_assets() {
        let preferred = Path::new("/usr/share/icons/Adwaita/scalable/apps/firefox.svg");
        let fallback = Path::new("/usr/share/icons/hicolor/48x48/apps/firefox.png");
        assert!(
            icon_path_score(preferred, Some("Adwaita"))
                < icon_path_score(fallback, Some("Adwaita"))
        );
    }

    #[test]
    fn resolves_and_loads_firefox_when_a_system_icon_is_available() {
        let Some(path) = resolve_icon_path("firefox") else {
            return;
        };
        assert!(
            Image::load_from_path(&path).is_ok(),
            "failed to load resolved icon {path:?}"
        );
    }
}
