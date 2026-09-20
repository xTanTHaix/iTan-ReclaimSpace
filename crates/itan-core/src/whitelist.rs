//! Target Boundary Rules — whitelist/blacklist for file types that may or must not
//! be deduplicated via hardlink.
//!
//! Immutable artifacts (libraries, static assets) are safe to hardlink because they
//! are never mutated in-place by build toolchains.  Volatile artifacts (incremental
//! compiler state, dependency maps) must never be hardlinked because their timestamps
//! and inode identity are load-bearing for incremental compilation correctness.

use std::path::Path;

/// Classification of a file with respect to hardlink eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    /// The file is an immutable artifact — safe to deduplicate.
    Whitelisted,
    /// The file is a volatile artifact — hardlinking is strictly prohibited.
    Prohibited,
    /// The file type is unrecognised; the engine skips it conservatively.
    Unknown,
}

/// Classifies `path` according to the Target Boundary Rules specified in §5 of the
/// iTan-ReclaimSpace v1.0 engineering specification.
///
/// Classification is performed on extension alone (case-insensitive) with one special
/// case: files whose **parent directory** is named `node_modules` are treated as
/// Whitelisted regardless of extension, reflecting pnpm's immutable package store model.
///
/// # Examples
///
/// ```
/// use itan_core::whitelist::{classify_path, FileClass};
/// use std::path::Path;
///
/// assert_eq!(classify_path(Path::new("libfoo.so")), FileClass::Whitelisted);
/// assert_eq!(classify_path(Path::new("foo.obj")), FileClass::Prohibited);
/// ```
pub fn classify_path(path: &Path) -> FileClass {
    // node_modules/ exception: any file directly inside this directory is whitelisted
    // because npm/pnpm packages are versioned and content-addressed by the package manager.
    if is_inside_node_modules(path) {
        return FileClass::Whitelisted;
    }

    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return FileClass::Unknown,
    };

    match ext.as_str() {
        // § 5.1 — Whitelisted Immutable Artifacts
        "a" | "lib"             // Static libraries
        | "so" | "dll" | "dylib" // Shared/dynamic libraries
        | "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" // Images
        | "mp3" | "ogg" | "wav" | "flac" | "aac"                  // Audio
        | "bin" | "dat" | "npz" | "gguf" | "onnx" | "pt" | "pth"  // AI models / data fixtures
        => FileClass::Whitelisted,

        // § 5.2 — Strictly Prohibited Volatile Artifacts
        "d"           // Dependency tree maps (gcc -MMD output)
        | "obj" | "o"  // Compiler intermediate objects
        | "pdb"        // Windows debug symbols
        | "tsbuildinfo" // TypeScript incremental cache
        => FileClass::Prohibited,

        // compile_commands.json is matched by filename, not extension.
        "json" if path
            .file_name()
            .and_then(|f| f.to_str())
            .map(|f| f.eq_ignore_ascii_case("compile_commands.json"))
            .unwrap_or(false)
        => FileClass::Prohibited,

        _ => FileClass::Unknown,
    }
}

/// Returns `true` if any ancestor directory component is named exactly `node_modules`.
fn is_inside_node_modules(path: &Path) -> bool {
    path.ancestors()
        .skip(1) // Skip the file itself
        .any(|ancestor| {
            ancestor
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == "node_modules")
                .unwrap_or(false)
        })
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // P3-U10: .dll must be whitelisted.
    #[test]
    fn test_whitelist_dll_allowed() {
        assert_eq!(classify_path(Path::new("foo.dll")), FileClass::Whitelisted);
    }

    // P3-U11: .obj must be prohibited.
    #[test]
    fn test_whitelist_obj_blocked() {
        assert_eq!(classify_path(Path::new("main.obj")), FileClass::Prohibited);
    }

    // P3-U12: .pdb must be prohibited.
    #[test]
    fn test_whitelist_pdb_blocked() {
        assert_eq!(classify_path(Path::new("app.pdb")), FileClass::Prohibited);
    }

    // Additional coverage:

    #[test]
    fn test_whitelist_static_lib_a_allowed() {
        assert_eq!(classify_path(Path::new("libssl.a")), FileClass::Whitelisted);
    }

    #[test]
    fn test_whitelist_lib_allowed() {
        assert_eq!(classify_path(Path::new("fmt.lib")), FileClass::Whitelisted);
    }

    #[test]
    fn test_whitelist_so_allowed() {
        assert_eq!(classify_path(Path::new("libz.so")), FileClass::Whitelisted);
    }

    #[test]
    fn test_whitelist_dylib_allowed() {
        assert_eq!(
            classify_path(Path::new("libfoo.dylib")),
            FileClass::Whitelisted
        );
    }

    #[test]
    fn test_whitelist_o_file_blocked() {
        assert_eq!(classify_path(Path::new("main.o")), FileClass::Prohibited);
    }

    #[test]
    fn test_whitelist_d_file_blocked() {
        assert_eq!(classify_path(Path::new("main.d")), FileClass::Prohibited);
    }

    #[test]
    fn test_whitelist_tsbuildinfo_blocked() {
        assert_eq!(
            classify_path(Path::new("tsconfig.tsbuildinfo")),
            FileClass::Prohibited
        );
    }

    #[test]
    fn test_whitelist_compile_commands_json_blocked() {
        assert_eq!(
            classify_path(Path::new("build/compile_commands.json")),
            FileClass::Prohibited
        );
    }

    #[test]
    fn test_whitelist_regular_json_unknown() {
        assert_eq!(classify_path(Path::new("package.json")), FileClass::Unknown);
    }

    #[test]
    fn test_whitelist_node_modules_any_file_whitelisted() {
        assert_eq!(
            classify_path(Path::new("node_modules/lodash/index.js")),
            FileClass::Whitelisted
        );
    }

    #[test]
    fn test_whitelist_node_modules_nested_whitelisted() {
        assert_eq!(
            classify_path(Path::new(
                "project/node_modules/.pnpm/react/dist/react.cjs.js"
            )),
            FileClass::Whitelisted
        );
    }

    #[test]
    fn test_whitelist_case_insensitive_extension() {
        // Extension matching must be case-insensitive.
        assert_eq!(classify_path(Path::new("FOO.DLL")), FileClass::Whitelisted);
        assert_eq!(classify_path(Path::new("main.OBJ")), FileClass::Prohibited);
    }

    #[test]
    fn test_whitelist_no_extension_unknown() {
        assert_eq!(classify_path(Path::new("Makefile")), FileClass::Unknown);
    }
}
