// SPDX-License-Identifier: MIT
//! `size-class-gen` — THE size-class generator (DD-1, W0-14d / W2-1).
//!
//! Reads the committed golden table `size-classes.json`, validates every
//! mathematical invariant the allocator and the proofs depend on (Section 9.3 /
//! 9.5 of `SPEC.md`), and emits three byte-for-byte consistent artifacts:
//!
//! * `crates/topo-core/src/generated/tables.rs` — consumed by the Rust runtime,
//! * `include/topomalloc_tables.h`              — consumed by C clients,
//! * `lean/TopoMalloc/Generated/SizeClasses.lean` — consumed by the Lean model.
//!
//! `xtask gen` runs this; `xtask gen --check` (and CI gate G-table) re-runs it
//! and fails on any drift from the committed output, so no table value is ever
//! hand-edited (Appendix F: "hand-maintained size-class tables").
//!
//! The generator validates only the *hard* correctness invariants (those that
//! make a table sound for any release). The §9.4 spacing-ratio *policy* targets
//! are proved in Lean (plan 02 W1-4b/c), the right place for regime-dependent
//! reasoning; here we additionally assert a loose universal sanity bound
//! (ratio ≤ 2.0) and emit the computed worst-case ratio per class as a metric.
#![allow(unreachable_pub)] // binary crate: `pub` items have no external surface

use std::path::PathBuf;
use std::process::ExitCode;

mod model;

use model::{Golden, Table};

/// Resolved output paths, relative to the repository root (the CWD `xtask` uses).
struct Paths {
    golden: PathBuf,
    out_rust: PathBuf,
    out_c: PathBuf,
    out_lean: PathBuf,
}

impl Paths {
    fn defaults() -> Self {
        Self {
            golden: PathBuf::from("tools/size-class-gen/size-classes.json"),
            out_rust: PathBuf::from("crates/topo-core/src/generated/tables.rs"),
            out_c: PathBuf::from("include/topomalloc_tables.h"),
            out_lean: PathBuf::from("lean/TopoMalloc/Generated/SizeClasses.lean"),
        }
    }
}

fn main() -> ExitCode {
    let mut check = false;
    let mut paths = Paths::defaults();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--golden" => paths.golden = require_value(&mut args, "--golden"),
            "--out-rust" => paths.out_rust = require_value(&mut args, "--out-rust"),
            "--out-c" => paths.out_c = require_value(&mut args, "--out-c"),
            "--out-lean" => paths.out_lean = require_value(&mut args, "--out-lean"),
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("size-class-gen: unknown argument '{other}'");
                print_help();
                return ExitCode::FAILURE;
            }
        }
    }

    match run(&paths, check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("size-class-gen: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn require_value(args: &mut impl Iterator<Item = String>, flag: &str) -> PathBuf {
    match args.next() {
        Some(v) => PathBuf::from(v),
        None => {
            eprintln!("size-class-gen: '{flag}' requires a value");
            std::process::exit(2);
        }
    }
}

fn print_help() {
    eprintln!(
        "usage: size-class-gen [--check] [--golden P] [--out-rust P] [--out-c P] [--out-lean P]\n\
         \n  --check   validate + regenerate in memory and fail if any output file differs\n\
         (no defaults override needed; paths default to the standard repo layout)"
    );
}

fn run(paths: &Paths, check: bool) -> Result<(), String> {
    let golden_src = std::fs::read_to_string(&paths.golden)
        .map_err(|e| format!("reading {}: {e}", paths.golden.display()))?;
    let golden: Golden = serde_json::from_str(&golden_src)
        .map_err(|e| format!("parsing {}: {e}", paths.golden.display()))?;
    let table = Table::validate(golden)?;

    // The Rust output is run through `rustfmt` so it is byte-identical to what
    // `cargo fmt` would produce. Otherwise `cargo fmt` (which formats every
    // `.rs` file, generated or not) would rewrite `tables.rs` and the G-table
    // golden-diff would fail forever. The `.h`/`.lean` outputs are untouched by
    // any formatter, so they are emitted verbatim.
    let outputs = [
        (&paths.out_rust, rustfmt_rust(&emit_rust(&table))),
        (&paths.out_c, emit_c(&table)),
        (&paths.out_lean, emit_lean(&table)),
    ];

    if check {
        let mut diverged = Vec::new();
        for (path, content) in &outputs {
            let current = std::fs::read_to_string(path).unwrap_or_default();
            if normalize(&current) != normalize(content) {
                diverged.push(path.display().to_string());
            }
        }
        if diverged.is_empty() {
            println!(
                "size-class-gen: OK — {} classes, all artifacts up to date",
                table.classes.len()
            );
            Ok(())
        } else {
            Err(format!(
                "generated artifacts are stale (G-table): {}. Run `cargo xtask gen` and commit.",
                diverged.join(", ")
            ))
        }
    } else {
        for (path, content) in &outputs {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            std::fs::write(path, content)
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
        }
        println!(
            "size-class-gen: wrote {} classes to {}, {}, {}",
            table.classes.len(),
            paths.out_rust.display(),
            paths.out_c.display(),
            paths.out_lean.display()
        );
        Ok(())
    }
}

/// Normalize line endings and a trailing newline so a check is robust to a
/// platform checkout rewriting CRLF (the content we emit always uses `\n`).
fn normalize(s: &str) -> String {
    let mut out = s.replace("\r\n", "\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Run the emitted Rust through `rustfmt` (using the repo's `rustfmt.toml`, found
/// via the working directory) so the committed `tables.rs` matches `cargo fmt`
/// exactly. If `rustfmt` is unavailable the source is returned unchanged — the
/// table is still valid Rust, it just may not match a later `cargo fmt`.
fn rustfmt_rust(src: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let child = Command::new("rustfmt")
        .args(["--edition", "2021", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return src.to_string(),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(src.as_bytes()).is_err() {
            return src.to_string();
        }
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => src.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Emitters. Each is a pure function of the validated table so it is unit
// testable and deterministic (stable field order, fixed formatting).
// ---------------------------------------------------------------------------

const GEN_BANNER: &str = "@generated by size-class-gen — DO NOT EDIT. \
Source of truth: tools/size-class-gen/size-classes.json. Regenerate: cargo xtask gen.";

fn emit_rust(t: &Table) -> String {
    let mut s = String::new();
    s.push_str("// SPDX-License-Identifier: MIT\n");
    s.push_str(&format!("//! {GEN_BANNER}\n//!\n"));
    s.push_str("//! Generated size-class tables consumed by the Rust runtime (plan 03).\n");
    s.push_str("#![allow(clippy::all)]\n\n");
    s.push_str("use crate::size_class::SizeClassRow;\n\n");
    s.push_str("/// Allocator page size in bytes.\n");
    s.push_str(&format!("pub const PAGE_SIZE: usize = {};\n", t.page_size));
    s.push_str("/// Alignment quantum (minimum class spacing) in bytes.\n");
    s.push_str(&format!("pub const QUANTUM: usize = {};\n", t.quantum));
    s.push_str("/// Smallest size class in bytes.\n");
    s.push_str(&format!("pub const TINY_MIN: usize = {};\n", t.tiny_min));
    s.push_str("/// Largest small-path request the table serves, in bytes.\n");
    s.push_str(&format!("pub const SMALL_MAX: usize = {};\n", t.small_max));
    s.push_str(
        "/// First request size served by the hugepage/region backend rather than the\n\
         /// small slab or medium extent path, in bytes (§9.2/§A.1/§18.5).\n",
    );
    s.push_str(&format!(
        "pub const HUGE_THRESHOLD: usize = {};\n",
        t.huge_threshold
    ));
    s.push_str(
        "/// Largest natural alignment any size class provides, in bytes. A request\n\
         /// needing more than this can never be served by a small class.\n",
    );
    s.push_str(&format!(
        "pub const MAX_ALIGN: usize = {};\n\n",
        t.max_align
    ));
    s.push_str("/// The size-class table (index = `SizeClassId`).\n");
    s.push_str("pub const SIZE_CLASSES: &[SizeClassRow] = &[\n");
    for c in &t.classes {
        s.push_str(&format!(
            "    SizeClassRow {{ size: {}, align: {}, slab_pages: {}, objects_per_slab: {}, \
             batch: {}, max_local_capacity: {} }},\n",
            c.size, c.align, c.slab_pages, c.objects_per_slab, c.batch, c.max_local_capacity
        ));
    }
    s.push_str("];\n\n");
    s.push_str("/// Direct-mapped lookup: granule `(size - 1) / QUANTUM` -> class index.\n");
    s.push_str("pub const SIZE_TO_CLASS: &[u8] = &[\n    ");
    let lut: Vec<String> = t.size_to_class.iter().map(|v| v.to_string()).collect();
    s.push_str(&lut.join(", "));
    s.push_str(",\n];\n");
    s
}

fn emit_c(t: &Table) -> String {
    let mut s = String::new();
    s.push_str("/* SPDX-License-Identifier: MIT */\n");
    s.push_str(&format!("/* {GEN_BANNER} */\n"));
    s.push_str("#ifndef TOPOMALLOC_TABLES_H\n#define TOPOMALLOC_TABLES_H\n\n");
    s.push_str("#include <stdint.h>\n\n");
    s.push_str(&format!("#define TOPOMALLOC_PAGE_SIZE {}u\n", t.page_size));
    s.push_str(&format!("#define TOPOMALLOC_QUANTUM {}u\n", t.quantum));
    s.push_str(&format!("#define TOPOMALLOC_TINY_MIN {}u\n", t.tiny_min));
    s.push_str(&format!("#define TOPOMALLOC_SMALL_MAX {}u\n", t.small_max));
    s.push_str(&format!(
        "#define TOPOMALLOC_HUGE_THRESHOLD {}u\n",
        t.huge_threshold
    ));
    s.push_str(&format!("#define TOPOMALLOC_MAX_ALIGN {}u\n", t.max_align));
    s.push_str(&format!(
        "#define TOPOMALLOC_NUM_SIZE_CLASSES {}u\n\n",
        t.classes.len()
    ));
    s.push_str("typedef struct topomalloc_size_class {\n");
    s.push_str("    uint32_t size;\n    uint32_t align;\n    uint32_t slab_pages;\n");
    s.push_str(
        "    uint32_t objects_per_slab;\n    uint32_t batch;\n    uint32_t max_local_capacity;\n",
    );
    s.push_str("} topomalloc_size_class_t;\n\n");
    s.push_str("static const topomalloc_size_class_t\n");
    s.push_str("    topomalloc_size_classes[TOPOMALLOC_NUM_SIZE_CLASSES] = {\n");
    for c in &t.classes {
        s.push_str(&format!(
            "        {{ {}u, {}u, {}u, {}u, {}u, {}u }},\n",
            c.size, c.align, c.slab_pages, c.objects_per_slab, c.batch, c.max_local_capacity
        ));
    }
    s.push_str("};\n\n");
    s.push_str(&format!(
        "static const uint8_t topomalloc_size_to_class[{}] = {{ ",
        t.size_to_class.len()
    ));
    let lut: Vec<String> = t.size_to_class.iter().map(|v| v.to_string()).collect();
    s.push_str(&lut.join(", "));
    s.push_str(" };\n\n");
    s.push_str("#endif /* TOPOMALLOC_TABLES_H */\n");
    s
}

fn emit_lean(t: &Table) -> String {
    let mut s = String::new();
    s.push_str("-- SPDX-License-Identifier: MIT\n");
    s.push_str(&format!("-- {GEN_BANNER}\n"));
    s.push_str("import TopoMalloc.SizeClass\n\n");
    s.push_str("namespace TopoMalloc.Generated\n\n");
    s.push_str(&format!("def pageSize : Nat := {}\n", t.page_size));
    s.push_str(&format!("def quantum : Nat := {}\n", t.quantum));
    s.push_str(&format!("def tinyMin : Nat := {}\n", t.tiny_min));
    s.push_str(&format!("def smallMax : Nat := {}\n", t.small_max));
    s.push_str(&format!(
        "def hugeThreshold : Nat := {}\n",
        t.huge_threshold
    ));
    s.push_str(&format!("def maxAlign : Nat := {}\n\n", t.max_align));
    s.push_str("def sizeClasses : List SizeClassRow := [\n");
    for (i, c) in t.classes.iter().enumerate() {
        let comma = if i + 1 < t.classes.len() { "," } else { "" };
        s.push_str(&format!(
            "  {{ size := {}, align := {}, slabPages := {}, objectsPerSlab := {}, \
             batch := {}, maxLocalCapacity := {} }}{comma}\n",
            c.size, c.align, c.slab_pages, c.objects_per_slab, c.batch, c.max_local_capacity
        ));
    }
    s.push_str("]\n\n");
    let lut: Vec<String> = t.size_to_class.iter().map(|v| v.to_string()).collect();
    s.push_str(&format!(
        "def sizeToClass : List Nat := [{}]\n\n",
        lut.join(", ")
    ));
    s.push_str("end TopoMalloc.Generated\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClassRow, Golden};

    fn trivial_golden() -> Golden {
        Golden {
            schema_version: 1,
            page_size: 16384,
            quantum: 16,
            tiny_min: 16,
            small_max: 32,
            huge_threshold: 2097152,
            classes: vec![
                ClassRow {
                    size: 16,
                    align: 16,
                    slab_pages: 1,
                    objects_per_slab: 1024,
                    batch: 32,
                    max_local_capacity: 256,
                },
                ClassRow {
                    size: 32,
                    align: 16,
                    slab_pages: 1,
                    objects_per_slab: 512,
                    batch: 32,
                    max_local_capacity: 256,
                },
            ],
        }
    }

    #[test]
    fn validates_and_builds_lookup() {
        let t = Table::validate(trivial_golden()).expect("valid");
        // small_max / quantum = 2 granules.
        assert_eq!(t.size_to_class, vec![0, 1]);
    }

    #[test]
    fn rejects_size_not_multiple_of_align() {
        let mut g = trivial_golden();
        g.classes[1].size = 33; // 33 % 16 != 0 (violates §9.3 third constraint)
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_align_not_power_of_two() {
        let mut g = trivial_golden();
        g.classes[0].align = 24;
        g.classes[0].size = 48; // multiple of 24 but 24 is not a power of two
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_non_monotonic_sizes() {
        let mut g = trivial_golden();
        g.classes.reverse();
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_wrong_objects_per_slab() {
        let mut g = trivial_golden();
        g.classes[0].objects_per_slab = 1000; // != 16384/16 = 1024
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_batch_over_capacity() {
        let mut g = trivial_golden();
        g.classes[0].batch = 1000; // > max_local_capacity 256
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_small_max_not_last_class() {
        let mut g = trivial_golden();
        g.small_max = 64; // last class is 32
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn emitters_are_deterministic() {
        let t = Table::validate(trivial_golden()).expect("valid");
        assert_eq!(emit_rust(&t), emit_rust(&t));
        assert_eq!(emit_c(&t), emit_c(&t));
        assert_eq!(emit_lean(&t), emit_lean(&t));
        assert!(emit_rust(&t).contains("SPDX-License-Identifier: MIT"));
        assert!(emit_c(&t).contains("TOPOMALLOC_TABLES_H"));
        assert!(emit_lean(&t).contains("TopoMalloc.Generated"));
        // The medium/large boundary and the derived max-alignment are emitted to
        // all three artifacts from the single source (DD-1).
        assert!(emit_rust(&t).contains("HUGE_THRESHOLD"));
        assert!(emit_rust(&t).contains("MAX_ALIGN"));
        assert!(emit_c(&t).contains("TOPOMALLOC_HUGE_THRESHOLD"));
        assert!(emit_c(&t).contains("TOPOMALLOC_MAX_ALIGN"));
        assert!(emit_lean(&t).contains("hugeThreshold"));
        assert!(emit_lean(&t).contains("maxAlign"));
    }
}
