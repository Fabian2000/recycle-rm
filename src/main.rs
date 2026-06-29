//! A from-scratch reimplementation of GNU coreutils `rm` (9.4).
//!
//! Goal: reproduce `rm`'s command-line behaviour; every option, exit code,
//! prompt and error message; 1:1, WITHOUT copying any GPL source (clean-room;
//! behaviour derived from the man page, POSIX and black-box observation, and
//! pinned down by tests/difftest.sh + tests/fuzz.sh).
//!
//! No code path here may panic: there are no `unwrap`/`expect`/`panic!` calls
//! and no fallible indexing in non-test code. Errors are always handled.
//!
//! The actual removal of a path is funnelled through a single, clearly marked
//! function: `destroy()`. That is the ONLY place a file would leave the
//! filesystem and the designated hook for step 2 (the recoverable trash). It
//! is currently a no-op placeholder; everything around it already behaves 1:1.

mod db;

use std::env;
use std::ffi::{CString, OsString};
use std::fs::{self, Metadata};
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use uuid::Uuid;

/// How interactively to prompt, matching GNU rm's --interactive=WHEN.
#[derive(Clone, Copy, PartialEq)]
enum Interactive {
    Never,
    Once,
    Always,
}

#[derive(Default)]
struct Options {
    /// ignore_missing_files: -f / --force. Sticky (suppresses ENOENT/ENOTDIR).
    force: bool,
    recursive: bool,
    dir: bool,
    verbose: bool,
    one_file_system: bool,
    preserve_root: bool,
    /// Final prompting mode (the last of -f/-i/-I/--interactive wins).
    interactive: Option<Interactive>,
}

enum Action {
    Remove { opts: Options, operands: Vec<OsString> },
    // Extensions, all reached via the `--trash <subcommand>` entry point.
    TrashList,
    TrashListPs,
    /// Restore one or more entries by id (or the literal "all").
    TrashRestore(Vec<String>),
    /// Permanently delete one or more entries by id (or "all").
    TrashDelete(Vec<String>),
    /// Show how much space the trash uses.
    TrashSize,
    /// List orphaned blobs (no DB row). With clear=true, delete them.
    TrashOrphans { clear: bool },
    /// Recover an orphan blob (no metadata) to a user-supplied destination.
    TrashOrphanRecover { name: String, dest: String },
    /// Permanently delete entries older than the retention period.
    TrashPrune,
    /// Show (None) or set (Some) the retention period in days ("off" disables).
    TrashRetention(Option<String>),
    /// Show (None) or set (Some) the soft max-size warning threshold.
    TrashMaxSize(Option<String>),
    /// Short help for the trash subcommands.
    TrashHelp,
    /// Turn trashing on/off persistently (off -> rm deletes permanently).
    TrashEnable(bool),
    /// Compress/decompress all entries (None) or one entry by id (Some).
    TrashCompress(Option<String>),
    TrashDecompress(Option<String>),
    TrashClear,
    ShowTrashTarget,
    SetTrashTarget(String),
    BlacklistList,
    BlacklistAdd(String),
    BlacklistRemove(String),
    WhitelistList,
    WhitelistAdd(String),
    WhitelistRemove(String),
    Help,
    Version,
}

fn main() -> ExitCode {
    // Rust sets SIGPIPE to SIG_IGN, which turns a closed pipe into an I/O error
    // that the print macros then panic on (e.g. `rm --trash ps | head`). Restore
    // the default disposition so we terminate cleanly by signal, like GNU tools.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }

    // Run on a thread with a large stack: the directory traversals are
    // recursive, so a pathologically deep tree could otherwise overflow the
    // default stack. Fall back to running inline if the thread can't spawn.
    match std::thread::Builder::new().stack_size(256 * 1024 * 1024).spawn(run) {
        Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
        Err(_) => run(),
    }
}

fn run() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    match parse_args(args) {
        Ok(Action::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Action::Version) => {
            print!("{VERSION}");
            ExitCode::SUCCESS
        }
        Ok(Action::SetTrashTarget(path)) => match set_trash_target(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("rm: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(Action::TrashList) => trash_action(list_trash(false)),
        Ok(Action::TrashListPs) => trash_action(list_trash(true)),
        Ok(Action::TrashRestore(ids)) => trash_action(restore_trash(&ids)),
        Ok(Action::TrashDelete(ids)) => trash_action(delete_entries(&ids)),
        Ok(Action::TrashSize) => trash_action(trash_size()),
        Ok(Action::TrashOrphans { clear }) => trash_action(trash_orphans(clear)),
        Ok(Action::TrashOrphanRecover { name, dest }) => {
            trash_action(trash_orphan_recover(&name, &dest))
        }
        Ok(Action::TrashPrune) => trash_action(trash_prune()),
        Ok(Action::TrashRetention(v)) => trash_action(trash_retention(v.as_deref())),
        Ok(Action::TrashMaxSize(v)) => trash_action(trash_max_size(v.as_deref())),
        Ok(Action::TrashHelp) => {
            print!("{TRASH_HELP}");
            ExitCode::SUCCESS
        }
        Ok(Action::TrashCompress(id)) => trash_action(recompress(id.as_deref(), true)),
        Ok(Action::TrashDecompress(id)) => trash_action(recompress(id.as_deref(), false)),
        Ok(Action::TrashClear) => trash_action(clear_trash()),
        Ok(Action::ShowTrashTarget) => trash_action(show_trash_target()),
        Ok(Action::BlacklistList) => trash_action(blacklist_show()),
        Ok(Action::BlacklistAdd(p)) => trash_action(blacklist_change(&p, true)),
        Ok(Action::BlacklistRemove(p)) => trash_action(blacklist_change(&p, false)),
        Ok(Action::TrashEnable(on)) => trash_action(set_trash_enabled(on)),
        Ok(Action::WhitelistList) => trash_action(whitelist_show()),
        Ok(Action::WhitelistAdd(p)) => trash_action(whitelist_change(&p, true)),
        Ok(Action::WhitelistRemove(p)) => trash_action(whitelist_change(&p, false)),
        Ok(Action::Remove { opts, operands }) => run_remove(&opts, &operands),
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

/// Map a trash-management result to an exit code, printing any error GNU-style.
fn trash_action(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rm: {e}");
            ExitCode::FAILURE
        }
    }
}

// ----------------------------------------------------------------------------
// Argument parsing (GNU rm compatible, including getopt_long prefix matching)
// ----------------------------------------------------------------------------

const TRY_HELP: &str = "Try 'rm --help' for more information.";

/// Canonical long option names, kept sorted so the "ambiguous" possibility
/// list comes out in the same order as GNU's.
const LONGS: [&str; 10] = [
    "dir",
    "force",
    "help",
    "interactive",
    "no-preserve-root",
    "one-file-system",
    "preserve-root",
    "recursive",
    "verbose",
    "version",
];

fn parse_args(args: Vec<OsString>) -> Result<Action, String> {
    let mut opts = Options { preserve_root: true, ..Default::default() };
    let mut operands: Vec<OsString> = Vec::new();
    let mut no_more_opts = false;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let lossy = arg.to_string_lossy().to_string();

        if no_more_opts || !lossy.starts_with('-') || lossy == "-" {
            operands.push(arg);
            continue;
        }

        if lossy == "--" {
            no_more_opts = true;
            continue;
        }

        if let Some(long) = lossy.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };

            // Our single extension entry point, matched EXACTLY (no
            // abbreviation), so it never perturbs GNU's option namespace:
            // '--t', '--tr', '--trashx' etc. stay "unrecognized" like GNU rm.
            // Everything after `--trash` is consumed as its subcommand argv.
            if name == "trash" {
                let mut rest: Vec<String> = Vec::new();
                if let Some(v) = value {
                    rest.push(v);
                }
                rest.extend(iter.by_ref().map(|a| a.to_string_lossy().to_string()));
                return parse_trash_subcommand(&rest);
            }

            // Resolve a possibly-abbreviated long option (getopt_long rules).
            let canon: &str = if LONGS.contains(&name) {
                name
            } else {
                let ms: Vec<&str> =
                    LONGS.iter().filter(|c| c.starts_with(name)).copied().collect();
                match ms.as_slice() {
                    [] => return Err(format!("rm: unrecognized option '--{long}'\n{TRY_HELP}")),
                    [only] => only,
                    many => {
                        let list =
                            many.iter().map(|m| format!("'--{m}'")).collect::<Vec<_>>().join(" ");
                        return Err(format!(
                            "rm: option '--{name}' is ambiguous; possibilities: {list}\n{TRY_HELP}"
                        ));
                    }
                }
            };

            // coreutils refuses any abbreviation of --no-preserve-root.
            if canon == "no-preserve-root" && name != "no-preserve-root" {
                return Err("rm: you may not abbreviate the --no-preserve-root option".into());
            }

            match canon {
                // --force, like -f, also makes the final prompt mode "never".
                "force" => {
                    opts.force = true;
                    opts.interactive = Some(Interactive::Never);
                }
                "recursive" => opts.recursive = true,
                "dir" => opts.dir = true,
                "verbose" => opts.verbose = true,
                "one-file-system" => opts.one_file_system = true,
                "no-preserve-root" => opts.preserve_root = false,
                "preserve-root" => opts.preserve_root = true,
                "interactive" => {
                    let when = match value.as_deref() {
                        None | Some("always") | Some("yes") => Interactive::Always,
                        Some("never") | Some("no") | Some("none") => Interactive::Never,
                        Some("once") => Interactive::Once,
                        Some(other) => {
                            return Err(format!(
                                "rm: invalid argument \u{2018}{other}\u{2019} for \u{2018}--interactive\u{2019}\n\
                                 Valid arguments are:\n\
                                 \u{20}\u{20}- \u{2018}never\u{2019}, \u{2018}no\u{2019}, \u{2018}none\u{2019}\n\
                                 \u{20}\u{20}- \u{2018}once\u{2019}\n\
                                 \u{20}\u{20}- \u{2018}always\u{2019}, \u{2018}yes\u{2019}\n\
                                 {TRY_HELP}"
                            ))
                        }
                    };
                    opts.interactive = Some(when);
                    // Selecting always/once (even via the long form) clears the
                    // -f "ignore missing" flag (never leaves it untouched).
                    if when != Interactive::Never {
                        opts.force = false;
                    }
                }
                "help" => return Ok(Action::Help),
                "version" => return Ok(Action::Version),
                _ => return Err(format!("rm: unrecognized option '--{long}'\n{TRY_HELP}")),
            }
            continue;
        }

        // Short options, possibly bundled (e.g. -rf, -riv).
        for ch in lossy.chars().skip(1) {
            match ch {
                'f' => {
                    opts.force = true;
                    opts.interactive = Some(Interactive::Never);
                }
                // Short -i/-I also clear the -f "ignore missing" flag.
                'i' => {
                    opts.interactive = Some(Interactive::Always);
                    opts.force = false;
                }
                'I' => {
                    opts.interactive = Some(Interactive::Once);
                    opts.force = false;
                }
                'r' | 'R' => opts.recursive = true,
                'd' => opts.dir = true,
                'v' => opts.verbose = true,
                _ => return Err(format!("rm: invalid option -- '{ch}'\n{TRY_HELP}")),
            }
        }
    }

    if operands.is_empty() && !opts.force {
        return Err(format!("rm: missing operand\n{TRY_HELP}"));
    }

    Ok(Action::Remove { opts, operands })
}

const TRASH_USAGE: &str = "usage: rm --trash <on|off|list|ps|size|restore <id>|delete <id>|clear|prune|compress [id]|decompress [id]|orphans [clear|recover NAME DEST]|target [PATH]|retention [days]|max-size [SIZE]|blacklist [add|rm PROG]|whitelist [add|rm PATH]>";

const TRASH_HELP: &str = "\
rm --trash <subcommand>  manage the recoverable trash

  on | off             enable/disable trashing (off = rm deletes permanently)
  list                 list trashed entries (ID, time, type, path)
  ps                   like list, also showing the program that deleted each
  size                 show entry count and disk usage
  restore <id>...      restore entries to their original path ('all' for every)
  delete  <id>...      permanently delete entries ('all' for every), with prompt
  clear                permanently empty the whole trash, with prompt
  prune                permanently delete entries past the retention period
  compress   [id]      pack entries as tar.zst ('all' if no id)
  decompress [id]      unpack compressed entries
  orphans [clear]      list (or delete) blobs with no database record
  orphans recover NAME DEST  move an orphan blob to a path you choose
  target [PATH]        show, or set, the storage location
  retention [days]     show, or set, auto-prune age (default 30, 'off' to disable)
  max-size [SIZE]      show, or set, a soft size limit that only warns (e.g. 5G)
  blacklist            show programs/scripts that bypass the trash
  blacklist add PROG   permanently delete (don't trash) when PROG calls rm
  blacklist rm  PROG   remove PROG from the blacklist
  whitelist            show the path whitelist (if set, only these are trashed)
  whitelist add PATH   trash only under PATH (everything else is deleted)
  whitelist rm  PATH   remove PATH from the whitelist

IDs are the short hex handles shown by 'list' (a leading '#' is allowed).

Note: this is an independent trash, NOT the freedesktop.org/desktop trash;
items here do not appear in your file manager's Trash, and vice versa.
";

/// Parse the words following `--trash` into a management action.
fn parse_trash_subcommand(args: &[String]) -> Result<Action, String> {
    let words: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match words.as_slice() {
        [] => Err(format!("rm: missing trash subcommand\n{TRASH_USAGE}")),
        ["list"] => Ok(Action::TrashList),
        ["ps"] => Ok(Action::TrashListPs),
        ["help"] => Ok(Action::TrashHelp),
        ["on"] => Ok(Action::TrashEnable(true)),
        ["off"] => Ok(Action::TrashEnable(false)),
        ["restore", ids @ ..] if !ids.is_empty() => {
            Ok(Action::TrashRestore(ids.iter().map(|s| s.to_string()).collect()))
        }
        ["delete", ids @ ..] if !ids.is_empty() => {
            Ok(Action::TrashDelete(ids.iter().map(|s| s.to_string()).collect()))
        }
        ["size"] => Ok(Action::TrashSize),
        ["orphans"] => Ok(Action::TrashOrphans { clear: false }),
        ["orphans", "clear"] => Ok(Action::TrashOrphans { clear: true }),
        ["orphans", "recover", name, dest] => Ok(Action::TrashOrphanRecover {
            name: (*name).to_string(),
            dest: (*dest).to_string(),
        }),
        ["prune"] => Ok(Action::TrashPrune),
        ["retention"] => Ok(Action::TrashRetention(None)),
        ["retention", v] => Ok(Action::TrashRetention(Some((*v).to_string()))),
        ["max-size"] => Ok(Action::TrashMaxSize(None)),
        ["max-size", v] => Ok(Action::TrashMaxSize(Some((*v).to_string()))),
        ["clear"] => Ok(Action::TrashClear),
        // compress/decompress: no id -> all entries, one id -> just that one.
        ["compress"] => Ok(Action::TrashCompress(None)),
        ["compress", id] => Ok(Action::TrashCompress(Some((*id).to_string()))),
        ["decompress"] => Ok(Action::TrashDecompress(None)),
        ["decompress", id] => Ok(Action::TrashDecompress(Some((*id).to_string()))),
        ["target"] => Ok(Action::ShowTrashTarget),
        ["target", path] => Ok(Action::SetTrashTarget((*path).to_string())),
        ["blacklist"] => Ok(Action::BlacklistList),
        ["blacklist", "add", prog] => Ok(Action::BlacklistAdd((*prog).to_string())),
        ["blacklist", "rm", prog] => Ok(Action::BlacklistRemove((*prog).to_string())),
        ["whitelist"] => Ok(Action::WhitelistList),
        ["whitelist", "add", path] => Ok(Action::WhitelistAdd((*path).to_string())),
        ["whitelist", "rm", path] => Ok(Action::WhitelistRemove((*path).to_string())),
        _ => Err(format!("rm: unknown trash subcommand '{}'\n{TRASH_USAGE}", args.join(" "))),
    }
}

// ----------------------------------------------------------------------------
// Trash location & configuration
// ----------------------------------------------------------------------------

/// The default trash home: $XDG_DATA_HOME/rm-trash, else ~/.local/share/rm-trash.
fn trash_home() -> PathBuf {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/share")
        });
    base.join("rm-trash")
}

/// The control database lives in a fixed location inside the default trash
/// home, so a configured alternative target can be looked up before we know
/// where the target is.
fn db_file() -> PathBuf {
    trash_home().join("rm.db")
}

/// Create the trash home if needed and lock it down to 0700 so other users on a
/// shared system cannot read trashed files or the metadata DB. Traversal of
/// this directory is denied to others, which also protects the default target.
fn ensure_trash_home() -> Result<(), String> {
    let home = trash_home();
    fs::create_dir_all(&home).map_err(|e| {
        format!("cannot create trash directory '{}': {}", home.display(), os_err(&e))
    })?;
    let _ = fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    Ok(())
}

/// The directory trashed files live in: the configured `--trash-target`, else
/// the built-in default `<trash_home>/files`. Does not create anything.
fn resolve_target(conn: &Connection) -> Result<PathBuf, String> {
    match db::get_setting(conn, "target") {
        Ok(Some(t)) if !t.is_empty() => Ok(PathBuf::from(t)),
        Ok(_) => Ok(trash_home().join("files")),
        Err(e) => Err(format!("cannot read trash target: {e}")),
    }
}

/// Persist an alternative storage location for trashed files (e.g. a mounted
/// Storage Box). An empty target resets to the built-in default.
///
/// The path is validated FIRST (existing, writable directory; relative paths
/// resolved to absolute). On any problem we error and leave the stored target
/// untouched, so a typo can never silently break later removals.
fn set_trash_target(path: &str) -> Result<(), String> {
    let resolved = if path.is_empty() {
        None
    } else {
        Some(validate_target(path)?)
    };

    ensure_trash_home()?;
    let conn = db::open(&db_file()).map_err(|e| format!("cannot open trash database: {e}"))?;

    match resolved {
        None => {
            db::set_setting(&conn, "target", "")
                .map_err(|e| format!("cannot save trash target: {e}"))?;
            println!("trash target reset to default '{}'", trash_home().display());
        }
        Some(abs) => {
            db::set_setting(&conn, "target", &abs)
                .map_err(|e| format!("cannot save trash target: {e}"))?;
            println!("trash target set to '{abs}'");
        }
    }
    Ok(())
}

/// Validate an alternative trash target, returning its absolute path. Rejects
/// nonexistent paths, non-directories and non-writable directories.
fn validate_target(path: &str) -> Result<String, String> {
    let abs = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("invalid trash target '{path}': {}", os_err(&e)))?;
    if !abs.is_dir() {
        return Err(format!("invalid trash target '{path}': Not a directory"));
    }
    probe_writable(&abs)?;
    Ok(abs.to_string_lossy().to_string())
}

/// Confirm a directory is writable by creating and removing a probe file;
/// robust against read-only mounts and correct even when running as root.
fn probe_writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(".rm-trash-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!("trash target '{}' is not writable: {}", dir.display(), os_err(&e))),
    }
}

/// List the trash: ID (hex handle), deletion time, type and original path.
/// With `verbose`, also show the program that invoked the removal (and its
/// full command line): the "ps" view.
fn list_trash(verbose: bool) -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        println!("Trash is empty.");
        return Ok(());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let entries = db::list(&conn).map_err(|e| format!("cannot read trash: {e}"))?;

    // Reconcile with reality: a user may have manually restored/removed a blob
    // from the trash directory. Such stale rows are pruned here. Guard: only do
    // this when the target directory is actually present (an absent target
    // like an unmounted Storage Box must NOT be mistaken for "all removed").
    let target = resolve_target(&conn)?;
    let target_present = fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false);

    // Grace window: a row is written just before its blob is moved in, so a
    // brand-new row may legitimately have no blob yet (or be mid cross-device
    // copy in another process). Only prune blob-less rows older than this.
    let cutoff = now_micros().saturating_sub(10 * 1_000_000);
    let entries: Vec<db::TrashEntry> = if target_present {
        entries
            .into_iter()
            .filter(|e| {
                let exists = fs::symlink_metadata(target.join(&e.id)).is_ok();
                if !exists && e.deleted_at < cutoff {
                    let _ = db::remove(&conn, &e.id); // drop the stale row
                    return false;
                }
                exists
            })
            .collect()
    } else {
        entries
    };

    if entries.is_empty() {
        println!("Trash is empty.");
        return Ok(());
    }

    if verbose {
        println!("{:<6} {:<26} {:<4} {:<14} ORIGINAL PATH", "ID", "DELETED", "TYPE", "CALLER");
    } else {
        println!("{:<6} {:<26} {:<4} ORIGINAL PATH", "ID", "DELETED", "TYPE");
    }
    for e in entries {
        let kind = if e.is_dir { "dir" } else { "file" };
        let id = format!("{:x}", e.seq);
        let when = format_time(e.deleted_at);
        if verbose {
            let caller = if e.caller.is_empty() { "?" } else { &e.caller };
            println!("{id:<6} {when:<26} {kind:<4} {caller:<14} {}", e.original_path);
            if !e.caller_cmdline.is_empty() {
                println!("{:<6} {:<26} {:<4} {:<14}    {}", "", "", "", "", e.caller_cmdline);
            }
        } else {
            println!("{id:<6} {when:<26} {kind:<4} {}", e.original_path);
        }
    }
    Ok(())
}

/// Find blobs in the target with no database row (e.g. after a lost DB). With
/// `clear`, permanently delete them after confirmation. Otherwise, just list.
fn trash_orphans(clear: bool) -> Result<(), String> {
    let dbf = db_file();
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }

    let known: std::collections::HashSet<String> = db::list(&conn)
        .map_err(|e| format!("cannot read trash: {e}"))?
        .into_iter()
        .map(|e| e.id)
        .collect();

    let rd = fs::read_dir(&target)
        .map_err(|e| format!("cannot read trash target: {}", os_err(&e)))?;
    let mut orphans: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip our own temp files (".restore-...", ".compress-...", probes).
        if name.starts_with('.') || known.contains(&name) {
            continue;
        }
        orphans.push(entry.path());
    }

    if orphans.is_empty() {
        println!("No orphaned blobs.");
        return Ok(());
    }

    let total: u64 = orphans.iter().map(|p| dir_size(p)).sum();
    println!("{} orphaned blob(s), {}:", orphans.len(), human_size(total));
    for p in &orphans {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        println!("  {name}  ({})", human_size(dir_size(p)));
    }

    if !clear {
        println!("(recover one with 'rm --trash orphans recover <name> <dest>', or delete all with 'rm --trash orphans clear')");
        return Ok(());
    }
    if !prompt(&format!("rm: permanently delete {} orphaned blob(s)? ", orphans.len())) {
        return Ok(());
    }
    for p in &orphans {
        let _ = remove_recursive(p);
    }
    println!("Removed {} orphaned blob(s).", orphans.len());
    Ok(())
}

/// Recover an orphan blob (which has no recorded original path) to a
/// user-supplied destination.
fn trash_orphan_recover(name: &str, dest: &str) -> Result<(), String> {
    // `name` must be a single trash-directory entry, never a path.
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(format!("invalid orphan name: '{name}'"));
    }
    let dbf = db_file();
    if !dbf.exists() {
        return Err(format!("no such orphan: '{name}'"));
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }

    let blob = target.join(name);
    if fs::symlink_metadata(&blob).is_err() {
        return Err(format!("no such blob in the trash: '{name}'"));
    }
    if db::id_exists(&conn, name).map_err(|e| format!("cannot read trash: {e}"))? {
        return Err(format!(
            "'{name}' is a tracked entry, not an orphan. Use 'rm --trash restore'"
        ));
    }

    let destp = PathBuf::from(dest);
    if fs::symlink_metadata(&destp).is_ok() {
        return Err(format!("target already exists: '{dest}'"));
    }
    if let Some(parent) = destp.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create '{}': {}", parent.display(), os_err(&e)))?;
    }
    move_path(&blob, &destp).map_err(|e| format!("cannot recover '{name}': {e}"))?;
    println!("recovered orphan '{name}' to '{dest}'");
    Ok(())
}

// ---- Retention & size limits ----------------------------------------------

/// Open the control DB, creating the trash home (0700) if needed.
fn open_control_db() -> Result<Connection, String> {
    ensure_trash_home()?;
    db::open(&db_file()).map_err(|e| format!("cannot open trash database: {e}"))
}

/// Persistently turn trashing on or off (stored in the DB).
fn set_trash_enabled(on: bool) -> Result<(), String> {
    let conn = open_control_db()?;
    db::set_setting(&conn, "enabled", if on { "on" } else { "off" })
        .map_err(|e| format!("cannot save setting: {e}"))?;
    if on {
        println!("trash enabled. Removed files go to the recoverable trash.");
    } else {
        println!("trash disabled. rm now deletes permanently (turn back on with 'rm --trash on').");
    }
    Ok(())
}

/// Whether trashing is enabled. Unset -> true, "off"/"0" -> false (rm behaves
/// like a normal permanent rm until the user turns the trash on).
fn trash_enabled(conn: &Connection) -> Result<bool, String> {
    match db::get_setting(conn, "enabled").map_err(|e| format!("cannot read settings: {e}"))? {
        Some(s) if s == "off" || s == "0" => Ok(false),
        _ => Ok(true),
    }
}

/// Configured retention in days. Unset -> default 30, "off"/"0" -> None (disabled).
fn retention_days(conn: &Connection) -> Result<Option<u64>, String> {
    match db::get_setting(conn, "retention_days").map_err(|e| format!("cannot read settings: {e}"))? {
        None => Ok(Some(30)),
        Some(s) if s == "off" || s == "0" => Ok(None),
        Some(s) => Ok(Some(s.parse().unwrap_or(30))),
    }
}

/// Configured soft max-size in bytes, or None if no limit is set.
fn max_size_bytes(conn: &Connection) -> Result<Option<u64>, String> {
    match db::get_setting(conn, "max_size_bytes").map_err(|e| format!("cannot read settings: {e}"))? {
        Some(s) if s != "0" && !s.is_empty() => Ok(s.parse().ok()),
        _ => Ok(None),
    }
}

/// Permanently remove entries deleted longer ago than `days`. Silent, returns
/// the number removed. Guards against an unavailable target.
fn prune_expired(conn: &Connection, target: &Path, days: u64) -> Result<usize, String> {
    if !fs::symlink_metadata(target).map(|m| m.is_dir()).unwrap_or(false) {
        return Ok(0);
    }
    let cutoff = now_micros()
        .saturating_sub((days as i64).saturating_mul(86_400).saturating_mul(1_000_000));
    let entries = db::list(conn).map_err(|e| format!("cannot read trash: {e}"))?;
    let mut n = 0;
    for e in entries {
        if e.deleted_at < cutoff {
            let _ = remove_recursive(&target.join(&e.id));
            db::remove(conn, &e.id).map_err(|err| format!("cannot update trash database: {err}"))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Print a warning if a max-size limit is configured and currently exceeded.
fn warn_if_over_quota(conn: &Connection, target: &Path) {
    if let Ok(Some(limit)) = max_size_bytes(conn) {
        let used = dir_size(target);
        if used > limit {
            eprintln!(
                "rm: warning: trash is {} (over the {} limit). See 'rm --trash prune/clear'",
                human_size(used),
                human_size(limit)
            );
        }
    }
}

/// `--trash prune`: explicitly apply retention now.
fn trash_prune() -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        println!("Trash is empty.");
        return Ok(());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }
    match retention_days(&conn)? {
        None => {
            println!("Retention is disabled. Nothing pruned.");
            Ok(())
        }
        Some(days) => {
            let n = prune_expired(&conn, &target, days)?;
            println!("Pruned {n} entr{} older than {days} days.", if n == 1 { "y" } else { "ies" });
            Ok(())
        }
    }
}

/// `--trash retention [days|off]`: show or set the retention period.
fn trash_retention(value: Option<&str>) -> Result<(), String> {
    match value {
        None => {
            let dbf = db_file();
            let days = if dbf.exists() {
                let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
                retention_days(&conn)?
            } else {
                Some(30)
            };
            match days {
                Some(n) => println!("retention: {n} days"),
                None => println!("retention: disabled"),
            }
            Ok(())
        }
        Some(v) => {
            let conn = open_control_db()?;
            if v == "off" || v == "0" {
                db::set_setting(&conn, "retention_days", "off")
                    .map_err(|e| format!("cannot save setting: {e}"))?;
                println!("retention: disabled");
            } else {
                let n: u64 = v.parse().map_err(|_| format!("invalid day count: '{v}'"))?;
                db::set_setting(&conn, "retention_days", &n.to_string())
                    .map_err(|e| format!("cannot save setting: {e}"))?;
                println!("retention: {n} days");
            }
            Ok(())
        }
    }
}

/// `--trash max-size [SIZE|off]`: show or set the soft size warning threshold.
fn trash_max_size(value: Option<&str>) -> Result<(), String> {
    match value {
        None => {
            let dbf = db_file();
            let limit = if dbf.exists() {
                let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
                max_size_bytes(&conn)?
            } else {
                None
            };
            match limit {
                Some(b) => println!("max-size: {} ({b} bytes)", human_size(b)),
                None => println!("max-size: unlimited"),
            }
            Ok(())
        }
        Some(v) => {
            let conn = open_control_db()?;
            if v == "off" || v == "0" {
                db::set_setting(&conn, "max_size_bytes", "0")
                    .map_err(|e| format!("cannot save setting: {e}"))?;
                println!("max-size: unlimited");
            } else {
                let b = parse_size(v)?;
                db::set_setting(&conn, "max_size_bytes", &b.to_string())
                    .map_err(|e| format!("cannot save setting: {e}"))?;
                println!("max-size: {}", human_size(b));
            }
            Ok(())
        }
    }
}

/// Parse a human size like "500M", "5GiB", "1024" (binary units) into bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_lowercase();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let suffix: String = t.chars().skip_while(|c| c.is_ascii_digit()).collect();
    let n: u64 = digits.parse().map_err(|_| format!("invalid size: '{s}'"))?;
    let mult: u64 = match suffix.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024u64.pow(2),
        "g" | "gb" | "gib" => 1024u64.pow(3),
        "t" | "tb" | "tib" => 1024u64.pow(4),
        other => return Err(format!("invalid size unit '{other}' in '{s}'")),
    };
    Ok(n.saturating_mul(mult))
}

/// Show how many entries the trash holds and how much space they use on disk.
fn trash_size() -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        println!("Trash is empty (0 B).");
        return Ok(());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let count = db::list(&conn).map_err(|e| format!("cannot read trash: {e}"))?.len();
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }
    let bytes = dir_size(&target);
    println!(
        "Trash: {count} entr{} using {} ({bytes} bytes) at {}",
        if count == 1 { "y" } else { "ies" },
        human_size(bytes),
        target.display()
    );
    Ok(())
}

/// Total size in bytes of all regular files under `path` (symlinks not
/// followed; counted as their own link size). Best-effort: unreadable entries
/// are skipped.
fn dir_size(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        meta.len()
    } else if ft.is_dir() {
        match fs::read_dir(path) {
            Ok(rd) => rd.flatten().map(|e| dir_size(&e.path())).sum(),
            Err(_) => 0,
        }
    } else {
        meta.len()
    }
}

/// Human-readable byte count (binary units).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Show the active trash storage location.
fn show_trash_target() -> Result<(), String> {
    let dbf = db_file();
    let custom = if dbf.exists() {
        let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
        db::get_setting(&conn, "target")
            .map_err(|e| format!("cannot read trash target: {e}"))?
            .filter(|t| !t.is_empty())
    } else {
        None
    };
    match custom {
        Some(t) => println!("trash target: {t} (custom)"),
        None => println!("trash target: {} (default)", trash_home().join("files").display()),
    }
    Ok(())
}

/// Single top-level member name used inside every trash archive, so extraction
/// reconstructs the original file/dir at a predictable path.
const ARCHIVE_MEMBER: &str = "entry";

/// Resolve a list of id arguments to entries. The literal "all" expands to the
/// whole trash; otherwise each argument is a hex handle (optional leading '#').
fn resolve_entries(conn: &Connection, ids: &[String]) -> Result<Vec<db::TrashEntry>, String> {
    if ids.iter().any(|i| i == "all") {
        return db::list(conn).map_err(|e| format!("cannot read trash: {e}"));
    }
    let mut out = Vec::new();
    for id in ids {
        let seq = i64::from_str_radix(id.trim_start_matches('#'), 16)
            .map_err(|_| format!("invalid trash id: '{id}'"))?;
        let entry = db::get(conn, seq)
            .map_err(|e| format!("cannot read trash: {e}"))?
            .ok_or_else(|| format!("no such trash entry: '{id}'"))?;
        out.push(entry);
    }
    Ok(out)
}

/// Restore one or more trashed entries back to their original paths. Recreates
/// missing parent directories and merges into an existing directory;
/// decompresses first if an entry is a compressed archive.
fn restore_trash(ids: &[String]) -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        return Err("trash is empty".into());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }

    let entries = resolve_entries(&conn, ids)?;
    let mut had_error = false;
    for entry in &entries {
        if let Err(e) = restore_one(&conn, &target, entry) {
            eprintln!("rm: {e}");
            had_error = true;
        }
    }
    if had_error {
        Err("one or more entries could not be restored".into())
    } else {
        Ok(())
    }
}

/// Restore a single entry (see `restore_trash`).
fn restore_one(conn: &Connection, target: &Path, entry: &db::TrashEntry) -> Result<(), String> {
    let blob = target.join(&entry.id);
    if fs::symlink_metadata(&blob).is_err() {
        return Err(format!("the data for entry '{:x}' is missing from the trash", entry.seq));
    }

    let dest = PathBuf::from(&entry.original_path);
    // Child-before-parent: recreate any missing parent directories.
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create '{}': {}", parent.display(), os_err(&e)))?;
    }

    if entry.compressed {
        // Decompress into a temp dir, then merge the reconstructed tree back.
        let tmp = target.join(format!(".restore-{}-{}", entry.id, std::process::id()));
        let _ = remove_recursive(&tmp);
        extract_archive(&blob, &tmp)?;
        let reconstructed = tmp.join(ARCHIVE_MEMBER);
        let result =
            check_no_conflict(&reconstructed, &dest).and_then(|()| restore_move(&reconstructed, &dest));
        let _ = remove_recursive(&tmp);
        result?;
        let _ = remove_recursive(&blob);
    } else {
        check_no_conflict(&blob, &dest)?;
        restore_move(&blob, &dest)?;
    }

    db::remove(conn, &entry.id).map_err(|e| format!("cannot update trash database: {e}"))?;
    println!("restored '{}'", dest.display());
    Ok(())
}

/// Verify that moving `src` onto `dest` would not clobber anything. Only a
/// directory-into-directory merge is allowed; any leaf collision is rejected.
fn check_no_conflict(src: &Path, dest: &Path) -> Result<(), String> {
    let dmeta = match fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(_) => return Ok(()), // dest free -> no conflict
    };
    let smeta = fs::symlink_metadata(src)
        .map_err(|e| format!("cannot restore '{}': {}", src.display(), os_err(&e)))?;
    if dmeta.is_dir() && smeta.is_dir() {
        let rd = fs::read_dir(src)
            .map_err(|e| format!("cannot read '{}': {}", src.display(), os_err(&e)))?;
        for entry in rd.flatten() {
            check_no_conflict(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        Err(format!("cannot restore: target already exists: '{}'", dest.display()))
    }
}

/// Move `src` onto `dest`, merging directories (assumes conflicts were already
/// ruled out by `check_no_conflict`).
fn restore_move(src: &Path, dest: &Path) -> Result<(), String> {
    if fs::symlink_metadata(dest).is_err() {
        return move_path(src, dest).map_err(|e| format!("cannot restore '{}': {e}", dest.display()));
    }
    // dest exists and (by pre-check) both are directories -> merge children in.
    let rd = fs::read_dir(src)
        .map_err(|e| format!("cannot read '{}': {}", src.display(), os_err(&e)))?;
    for entry in rd.flatten() {
        restore_move(&entry.path(), &dest.join(entry.file_name()))?;
    }
    let _ = fs::remove_dir(src); // remove the now-emptied source directory
    Ok(())
}

/// Compress (`compress` = true) or decompress trashed entries (all of them,
/// or just the one named by `id`: a hex handle). New deletions are never
/// compressed automatically; this is the explicit, on-demand control.
fn recompress(id: Option<&str>, compress: bool) -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        println!("Trash is empty.");
        return Ok(());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }

    // Select the entries to act on.
    let entries = match id {
        Some(id) => {
            let seq = i64::from_str_radix(id.trim_start_matches('#'), 16)
                .map_err(|_| format!("invalid trash id: '{id}'"))?;
            let e = db::get(&conn, seq)
                .map_err(|e| format!("cannot read trash: {e}"))?
                .ok_or_else(|| format!("no such trash entry: '{id}'"))?;
            vec![e]
        }
        None => db::list(&conn).map_err(|e| format!("cannot read trash: {e}"))?,
    };

    let targeted = id.is_some();
    let mut changed = 0usize;
    let mut failed = 0usize;
    for entry in &entries {
        // An entry already in the desired state is a no-op. For a targeted id
        // that is an error (compress on compressed / decompress on plain); for
        // the bulk "all" case we simply skip it.
        if entry.compressed == compress {
            if targeted {
                let what = if compress { "already compressed" } else { "not compressed" };
                return Err(format!("trash entry '{:x}' is {what}", entry.seq));
            }
            continue;
        }
        let blob = target.join(&entry.id);
        if fs::symlink_metadata(&blob).is_err() {
            continue; // data gone (manually removed). Leave for list to prune
        }
        let r = if compress {
            compress_blob_in_place(&blob, &target)
        } else {
            decompress_blob_in_place(&blob, &target)
        };
        match r {
            Ok(()) => {
                db::set_compressed(&conn, &entry.id, compress)
                    .map_err(|e| format!("cannot update trash database: {e}"))?;
                changed += 1;
            }
            // A targeted single id fails hard. In bulk, we report and continue,
            // so smaller entries can still be processed even if a big one fails.
            Err(e) if targeted => return Err(e),
            Err(e) => {
                eprintln!("rm: {:x}: {e}", entry.seq);
                failed += 1;
            }
        }
    }

    let verb = if compress { "compressed" } else { "decompressed" };
    if failed > 0 {
        println!("{verb} {changed}, {failed} failed.");
        Err(format!("{failed} entr{} could not be {verb}", if failed == 1 { "y" } else { "ies" }))
    } else {
        println!("{verb} {changed} entr{}.", if changed == 1 { "y" } else { "ies" });
        Ok(())
    }
}

/// Replace a plain blob with a tar.zst archive (single member `ARCHIVE_MEMBER`).
fn compress_blob_in_place(blob: &Path, target: &Path) -> Result<(), String> {
    let tmp = target.join(format!(".compress-{}", unique_suffix(blob)));
    let result = (|| {
        let file = fs::File::create(&tmp)
            .map_err(|e| format!("cannot create archive: {}", os_err(&e)))?;
        let encoder = zstd::stream::write::Encoder::new(file, 19)
            .map_err(|e| format!("cannot init zstd: {}", os_err(&e)))?;
        let mut builder = tar::Builder::new(encoder);
        builder.follow_symlinks(false);
        let meta = fs::symlink_metadata(blob)
            .map_err(|e| format!("cannot read '{}': {}", blob.display(), os_err(&e)))?;
        if meta.is_dir() {
            builder
                .append_dir_all(ARCHIVE_MEMBER, blob)
                .map_err(|e| format!("cannot archive directory: {}", os_err(&e)))?;
        } else {
            let mut f = fs::File::open(blob)
                .map_err(|e| format!("cannot open '{}': {}", blob.display(), os_err(&e)))?;
            builder
                .append_file(ARCHIVE_MEMBER, &mut f)
                .map_err(|e| format!("cannot archive file: {}", os_err(&e)))?;
        }
        // Explicitly finish BOTH layers so a flush error (e.g. ENOSPC) is
        // caught here, not silently swallowed in a Drop. That would leave a
        // truncated archive that looks successful.
        let encoder = builder
            .into_inner()
            .map_err(|e| format!("cannot finish archive: {}", os_err(&e)))?;
        encoder.finish().map_err(|e| format!("cannot finish compression: {}", os_err(&e)))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Swap the archive in for the original blob.
    remove_recursive(blob).map_err(|e| format!("cannot replace blob: {}", os_err(&e)))?;
    fs::rename(&tmp, blob).map_err(|e| format!("cannot replace blob: {}", os_err(&e)))?;
    Ok(())
}

/// Replace a tar.zst archive blob with its extracted contents (reverse of
/// `compress_blob_in_place`).
fn decompress_blob_in_place(blob: &Path, target: &Path) -> Result<(), String> {
    let tmp = target.join(format!(".decompress-{}", unique_suffix(blob)));
    let _ = remove_recursive(&tmp);
    let result = extract_archive(blob, &tmp);
    if let Err(e) = result {
        let _ = remove_recursive(&tmp);
        return Err(e);
    }
    let reconstructed = tmp.join(ARCHIVE_MEMBER);
    // Swap the reconstructed entry in for the archive blob.
    if let Err(e) = remove_recursive(blob).map_err(|e| format!("cannot replace archive: {}", os_err(&e))) {
        let _ = remove_recursive(&tmp);
        return Err(e);
    }
    let r = fs::rename(&reconstructed, blob)
        .map_err(|e| format!("cannot replace archive: {}", os_err(&e)));
    let _ = remove_recursive(&tmp);
    r
}

/// A filesystem-safe unique suffix for temp names (PID + blob file name).
fn unique_suffix(blob: &Path) -> String {
    let name = blob.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    format!("{}-{name}", std::process::id())
}

/// Extract a tar.zst archive into `into_dir`, preserving metadata.
fn extract_archive(archive: &Path, into_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(archive)
        .map_err(|e| format!("cannot open archive '{}': {}", archive.display(), os_err(&e)))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|e| format!("cannot read archive '{}': {}", archive.display(), os_err(&e)))?;
    let mut tar = tar::Archive::new(decoder);
    fs::create_dir_all(into_dir)
        .map_err(|e| format!("cannot create '{}': {}", into_dir.display(), os_err(&e)))?;
    tar.unpack(into_dir)
        .map_err(|e| format!("cannot extract archive '{}': {}", archive.display(), os_err(&e)))
}

/// Permanently delete one or more trashed entries (by hex handle, or "all"),
/// after a single yes/no confirmation. Removes both blob and database row.
fn delete_entries(ids: &[String]) -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        return Err("trash is empty".into());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!("trash target '{}' is not available", target.display()));
    }

    let entries = resolve_entries(&conn, ids)?;
    if entries.is_empty() {
        println!("Nothing to delete.");
        return Ok(());
    }

    // One detailed prompt for a single entry, a summary prompt for many.
    let proceed = if entries.len() == 1 {
        let e = &entries[0];
        let kind = if e.is_dir { "directory" } else { "file" };
        prompt(&format!("rm: permanently delete trashed {kind} '{}'? ", e.original_path))
    } else {
        prompt(&format!("rm: permanently delete {} trashed item(s)? ", entries.len()))
    };
    if !proceed {
        return Ok(());
    }

    let mut had_error = false;
    for entry in &entries {
        let _ = remove_recursive(&target.join(&entry.id)); // best-effort
        if let Err(e) = db::remove(&conn, &entry.id) {
            eprintln!("rm: cannot update trash database: {e}");
            had_error = true;
        } else {
            println!("permanently deleted '{}'", entry.original_path);
        }
    }
    if had_error {
        Err("one or more entries could not be deleted".into())
    } else {
        Ok(())
    }
}

/// Permanently empty the trash, after a yes/no confirmation.
fn clear_trash() -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        println!("Trash is already empty.");
        return Ok(());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let entries = db::list(&conn).map_err(|e| format!("cannot read trash: {e}"))?;
    if entries.is_empty() {
        println!("Trash is already empty.");
        return Ok(());
    }

    // Guard: never clear when the target is unavailable (e.g. unmounted box);
    // we would drop DB rows for blobs that still exist elsewhere.
    let target = resolve_target(&conn)?;
    if !fs::symlink_metadata(&target).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(format!(
            "trash target '{}' is not available. Refusing to clear",
            target.display()
        ));
    }

    if !prompt(&format!(
        "rm: permanently delete all {} trashed item(s)? ",
        entries.len()
    )) {
        return Ok(());
    }

    let mut removed = 0usize;
    for e in &entries {
        let _ = remove_recursive(&target.join(&e.id)); // best-effort
        let _ = db::remove(&conn, &e.id);
        removed += 1;
    }
    println!("Trash emptied ({removed} item(s) permanently deleted).");
    Ok(())
}

/// Show the permanent-delete blacklist.
fn blacklist_show() -> Result<(), String> {
    let dbf = db_file();
    if !dbf.exists() {
        println!("Blacklist is empty.");
        return Ok(());
    }
    let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
    let progs = db::blacklist_list(&conn).map_err(|e| format!("cannot read blacklist: {e}"))?;
    if progs.is_empty() {
        println!("Blacklist is empty.");
        return Ok(());
    }
    println!("Programs that permanently delete instead of trashing:");
    for p in progs {
        println!("  {p}");
    }
    Ok(())
}

/// Add (`add` = true) or remove a program from the blacklist.
fn blacklist_change(prog: &str, add: bool) -> Result<(), String> {
    if prog.is_empty() {
        return Err("blacklist program name must not be empty".into());
    }
    let conn = open_control_db()?;
    if add {
        db::blacklist_add(&conn, prog).map_err(|e| format!("cannot update blacklist: {e}"))?;
        println!("'{prog}' will now be permanently deleted (not trashed).");
    } else {
        let removed =
            db::blacklist_remove(&conn, prog).map_err(|e| format!("cannot update blacklist: {e}"))?;
        if removed {
            println!("'{prog}' removed from the blacklist (will be trashed again).");
        } else {
            println!("'{prog}' was not on the blacklist.");
        }
    }
    Ok(())
}

/// Show the path whitelist.
fn whitelist_show() -> Result<(), String> {
    let dbf = db_file();
    let paths = if dbf.exists() {
        let conn = db::open(&dbf).map_err(|e| format!("cannot open trash database: {e}"))?;
        db::whitelist_list(&conn).map_err(|e| format!("cannot read whitelist: {e}"))?
    } else {
        Vec::new()
    };
    if paths.is_empty() {
        println!("Whitelist is empty (everything is trashed).");
        return Ok(());
    }
    println!("Only paths under these directories are trashed (the rest is deleted permanently):");
    for p in paths {
        println!("  {p}");
    }
    Ok(())
}

/// Add (`add` = true) or remove a path prefix from the whitelist. Added paths
/// are stored absolute (must exist).
fn whitelist_change(path: &str, add: bool) -> Result<(), String> {
    if path.is_empty() {
        return Err("whitelist path must not be empty".into());
    }
    let conn = open_control_db()?;
    if add {
        let abs = Path::new(path)
            .canonicalize()
            .map_err(|e| format!("invalid whitelist path '{path}': {}", os_err(&e)))?;
        let abs = abs.to_string_lossy().to_string();
        db::whitelist_add(&conn, &abs).map_err(|e| format!("cannot update whitelist: {e}"))?;
        println!("only paths under '{abs}' will be trashed now (others deleted permanently).");
    } else {
        // Remove by the given path as-is and, if it exists, its absolute form.
        let mut removed = db::whitelist_remove(&conn, path)
            .map_err(|e| format!("cannot update whitelist: {e}"))?;
        if let Ok(abs) = Path::new(path).canonicalize() {
            removed |= db::whitelist_remove(&conn, &abs.to_string_lossy())
                .map_err(|e| format!("cannot update whitelist: {e}"))?;
        }
        if removed {
            println!("'{path}' removed from the whitelist.");
        } else {
            println!("'{path}' was not on the whitelist.");
        }
    }
    Ok(())
}

/// Format a microsecond Unix timestamp as local time "YYYY-MM-DD HH:MM:SS.ffffff".
/// Falls back to the raw value if the platform conversion fails (never panics).
fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let usec = micros.rem_euclid(1_000_000);
    let mut tm = Tm::zeroed();
    let fmt = match CString::new("%Y-%m-%d %H:%M:%S") {
        Ok(f) => f,
        Err(_) => return format!("{micros}us"),
    };
    let mut buf = [0u8; 32];
    unsafe {
        if localtime_r(&secs, &mut tm).is_null() {
            return format!("{micros}us");
        }
        let n = strftime(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt.as_ptr(), &tm);
        if n == 0 {
            return format!("{micros}us");
        }
        let stamp = String::from_utf8_lossy(&buf[..n]);
        format!("{stamp}.{usec:06}")
    }
}

// ----------------------------------------------------------------------------
// Removal engine
// ----------------------------------------------------------------------------

fn run_remove(opts: &Options, operands: &[OsString]) -> ExitCode {
    let tty = io::stdin().is_terminal();

    // GNU rm -I: prompt once when removing recursively or more than 3 operands.
    // Gated purely on the final interactive mode (force already maps to Never).
    if opts.interactive == Some(Interactive::Once) && (opts.recursive || operands.len() > 3) {
        let suffix = if opts.recursive { " recursively" } else { "" };
        let msg = format!(
            "rm: remove {} argument{}{suffix}? ",
            operands.len(),
            if operands.len() == 1 { "" } else { "s" }
        );
        if !prompt(&msg) {
            return ExitCode::SUCCESS;
        }
    }

    // The program that invoked us (parent process). Determined once.
    let caller = caller_comm().unwrap_or_default();

    // The trash session is opened lazily on the first operand actually being
    // removed, so pure-error invocations (e.g. `rm nonexistent`) touch nothing.
    let mut session: Option<Session> = None;
    let mut had_error = false;

    for operand in operands {
        let path = PathBuf::from(operand);
        let disp = operand.as_bytes();
        let outcome = process(&path, disp, opts, tty, None);
        if outcome.had_error {
            had_error = true;
        }
        // `removed` -> act on the whole node; `partial` (--one-file-system) ->
        // act only on the same-device parts, leaving mounts in place.
        if !outcome.removed && !outcome.partial {
            continue;
        }
        if session.is_none() {
            match Session::open(&caller) {
                Ok(s) => session = Some(s),
                Err(e) => {
                    eprintln!("rm: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        let Some(s) = &session else { continue };

        // Per-operand decision: blacklisted caller, or (with an active path
        // whitelist) a path outside it -> permanent. Otherwise trash.
        let permanent = s.is_permanent(&path);
        let result = match (permanent, outcome.partial) {
            (true, false) => permanent_delete(&path, disp),
            (true, true) => permanent_delete_partial(&path, disp),
            (false, false) => s.trash(&path, disp),
            (false, true) => s.trash_partial(&path, disp),
        };
        if let Err(e) = result {
            eprintln!("rm: {e}");
            had_error = true;
        }
    }

    // After trashing, warn if the trash now exceeds a configured size limit.
    if let Some(s) = &session {
        warn_if_over_quota(&s.conn, &s.target);
    }

    if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// Per-invocation trash context: an open DB, the resolved target, and the
/// policy (blacklisted caller, path whitelist) used to decide trash vs. delete.
struct Session {
    conn: Connection,
    target: PathBuf,
    caller: String,
    /// The calling program/script is blacklisted -> everything goes permanent.
    blacklisted: bool,
    /// Active path whitelist (absolute prefixes). Empty -> trash everything.
    whitelist: Vec<PathBuf>,
    /// Master switch: when false, everything is deleted permanently.
    enabled: bool,
}

impl Session {
    fn open(caller: &str) -> Result<Self, String> {
        ensure_trash_home()?;
        let conn = db::open(&db_file()).map_err(|e| format!("cannot open trash database: {e}"))?;

        let enabled = trash_enabled(&conn)?;
        let mut blacklisted = false;
        for cand in caller_candidates(caller) {
            if db::is_blacklisted(&conn, &cand).map_err(|e| format!("cannot read blacklist: {e}"))? {
                blacklisted = true;
                break;
            }
        }
        let whitelist = db::whitelist_list(&conn)
            .map_err(|e| format!("cannot read whitelist: {e}"))?
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let target = resolve_target(&conn)?;
        fs::create_dir_all(&target).map_err(|e| {
            format!("cannot create trash target '{}': {}", target.display(), os_err(&e))
        })?;

        // Lazy retention: silently drop entries past the retention period.
        if let Ok(Some(days)) = retention_days(&conn) {
            let _ = prune_expired(&conn, &target, days);
        }

        Ok(Self { conn, target, caller: caller.to_string(), blacklisted, whitelist, enabled })
    }

    /// Whether this operand should be permanently deleted instead of trashed.
    fn is_permanent(&self, path: &Path) -> bool {
        if !self.enabled || self.blacklisted {
            return true;
        }
        if !self.whitelist.is_empty() {
            let abs = original_abs(path);
            return !self.whitelist.iter().any(|w| abs.starts_with(w));
        }
        false
    }
}

/// Permanently remove a path (blacklisted caller, or outside an active whitelist).
fn permanent_delete(path: &Path, disp: &[u8]) -> Result<(), String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot remove {}: {}", quote_name(disp), os_err(&e)))?;
    let r = if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    r.map_err(|e| format!("cannot remove {}: {}", quote_name(disp), os_err(&e)))
}

/// Outcome of processing one path: whether a diagnostic was emitted and whether
/// the top node was (logically) removed (i.e. should be physically trashed).
struct Outcome {
    had_error: bool,
    /// The whole node is removable as a unit (wholesale move).
    removed: bool,
    /// --one-file-system: the node has both removable parts AND skipped mounts,
    /// so it needs a device-aware partial move instead of a wholesale one.
    partial: bool,
}

impl Outcome {
    fn error() -> Self {
        Self { had_error: true, removed: false, partial: false }
    }
    fn skipped() -> Self {
        Self { had_error: false, removed: false, partial: false }
    }
    fn removed() -> Self {
        Self { had_error: false, removed: true, partial: false }
    }
}

/// Process one path. `disp` is the path spelled exactly as the user/parent
/// presented it (GNU shows operands verbatim, e.g. a trailing slash is kept).
/// Prints any GNU-style diagnostic itself; returns whether an error occurred.
fn process(path: &Path, disp: &[u8], opts: &Options, tty: bool, root_dev: Option<u64>) -> Outcome {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            let code = e.raw_os_error();
            // -f ignores nonexistent paths (ENOENT) and ENOTDIR components.
            if opts.force && (code == Some(2) || code == Some(20)) {
                return Outcome::skipped();
            }
            let text = if code == Some(2) {
                "No such file or directory".to_string()
            } else {
                os_err(&e)
            };
            eprintln!("rm: cannot remove {}: {text}", quote_name(disp));
            return Outcome::error();
        }
    };

    let is_dir = meta.is_dir();

    // --one-file-system: skip any directory that sits on a different filesystem
    // than the command-line argument (root_dev is set for descendants only).
    if opts.one_file_system && is_dir && root_dev.is_some_and(|r| r != meta.dev()) {
        eprintln!("rm: skipping {}, since it's on a different device", quote_name(disp));
        return Outcome::error();
    }
    // The device every descendant is compared against.
    let root_dev = root_dev.unwrap_or(meta.dev());

    if opts.recursive && opts.preserve_root && is_root_path(path) {
        eprintln!(
            "rm: it is dangerous to operate recursively on {}\n\
             rm: use --no-preserve-root to override this failsafe",
            quote_name(disp)
        );
        return Outcome::error();
    }

    if is_dir && !opts.recursive {
        if !opts.dir {
            eprintln!("rm: cannot remove {}: Is a directory", quote_name(disp));
            return Outcome::error();
        }
        if dir_has_entries(path) {
            eprintln!("rm: cannot remove {}: Directory not empty", quote_name(disp));
            return Outcome::error();
        }
        if !removal_prompt(path, disp, &meta, opts, tty) {
            return Outcome::skipped();
        }
        if opts.verbose {
            println!("removed directory {}", quote_name(disp));
        }
        return Outcome::removed();
    }

    if is_dir {
        // Recursive directory: refuse a trailing "." / ".." component.
        if last_is_dot(disp) {
            eprintln!("rm: refusing to remove '.' or '..' directory: skipping {}", quote_name(disp));
            return Outcome::error();
        }
        // GNU only asks to descend into a NON-empty directory. An empty one
        // goes straight to the "remove directory" prompt below.
        if opts.interactive == Some(Interactive::Always)
            && dir_has_entries(path)
            && !prompt(&format!("rm: descend into directory {}? ", quote_name(disp)))
        {
            return Outcome::skipped();
        }
        let mut had = false;
        let mut not_all_removed = false; // a child was skipped/partial (one-file-system)
        match fs::read_dir(path) {
            Ok(rd) => {
                // Build the child's display bytes: parent (trailing '/' trimmed)
                // + '/' + child name, all as raw bytes.
                let prefix: &[u8] = match disp.iter().rposition(|&b| b != b'/') {
                    Some(i) => &disp[..=i],
                    None => disp,
                };
                for entry in rd.flatten() {
                    let mut child_disp = prefix.to_vec();
                    child_disp.push(b'/');
                    child_disp.extend_from_slice(entry.file_name().as_bytes());
                    let o = process(&entry.path(), &child_disp, opts, tty, Some(root_dev));
                    had |= o.had_error;
                    if !o.removed {
                        not_all_removed = true;
                    }
                }
            }
            Err(e) => {
                eprintln!("rm: cannot remove {}: {}", quote_name(disp), os_err(&e));
                return Outcome::error();
            }
        }
        // --one-file-system: if a descendant was skipped (mount), this directory
        // is only partially removable. It is left in place (no "removed
        // directory" line), but its removable parts still go to the trash.
        if opts.one_file_system && not_all_removed {
            return Outcome { had_error: had, removed: false, partial: true };
        }
        if !removal_prompt(path, disp, &meta, opts, tty) {
            return Outcome { had_error: had, removed: false, partial: false };
        }
        if opts.verbose {
            println!("removed directory {}", quote_name(disp));
        }
        return Outcome { had_error: had, removed: true, partial: false };
    }

    // regular file, symlink or special file
    if !removal_prompt(path, disp, &meta, opts, tty) {
        return Outcome::skipped();
    }
    if opts.verbose {
        println!("removed {}", quote_name(disp));
    }
    Outcome::removed()
}

/// Decide whether to remove `path`, prompting exactly like GNU rm. The decision
/// derives solely from the final interactive mode (force already maps to Never).
fn removal_prompt(path: &Path, disp: &[u8], meta: &Metadata, opts: &Options, tty: bool) -> bool {
    match opts.interactive {
        Some(Interactive::Always) => {
            let wp = if is_write_protected(path) { "write-protected " } else { "" };
            prompt(&format!("rm: remove {wp}{} {}? ", descriptor(meta), quote_name(disp)))
        }
        Some(Interactive::Never) => true,
        // -I (Once) and the default both fall back to the tty-gated protection.
        Some(Interactive::Once) | None => default_prompt(path, disp, meta, tty),
    }
}

/// GNU rm's default safeguard: only prompts for write-protected targets, and
/// only when standard input is a terminal. With piped stdin it never prompts.
fn default_prompt(path: &Path, disp: &[u8], meta: &Metadata, tty: bool) -> bool {
    if tty && is_write_protected(path) {
        return prompt(&format!(
            "rm: remove write-protected {} {}? ",
            descriptor(meta),
            quote_name(disp)
        ));
    }
    true
}

/// GNU's noun for each file type, used in interactive prompts.
fn descriptor(meta: &Metadata) -> &'static str {
    let ft = meta.file_type();
    if ft.is_symlink() {
        "symbolic link"
    } else if ft.is_dir() {
        "directory"
    } else if ft.is_fifo() {
        "fifo"
    } else if ft.is_socket() {
        "socket"
    } else if ft.is_char_device() {
        "character special file"
    } else if ft.is_block_device() {
        "block special file"
    } else if meta.len() == 0 {
        "regular empty file"
    } else {
        "regular file"
    }
}

// ============================================================================
// Moving an operand into the trash and recording its origin.
// ============================================================================

impl Session {
    /// Move one operand into the trash under a fresh UUIDv7 name and record its
    /// original absolute path with a microsecond-precision deletion timestamp.
    fn trash(&self, path: &Path, disp: &[u8]) -> Result<(), String> {
        let meta = fs::symlink_metadata(path)
            .map_err(|e| format!("cannot remove {}: {}", quote_name(disp), os_err(&e)))?;

        // Refuse to trash a path that contains the trash target. Otherwise, we
        // would move the trash directory into itself.
        if let (Ok(tc), Ok(pc)) = (self.target.canonicalize(), path.canonicalize())
            && tc.starts_with(&pc)
        {
            return Err(format!(
                "cannot remove {}: it contains the trash directory '{}'",
                quote_name(disp),
                self.target.display()
            ));
        }

        let original = original_abs(path).to_string_lossy().to_string();

        // Cross-filesystem moves are a physical copy and need free space in the
        // target. A same-fs rename needs none. Pre-flight check so we fail fast
        // instead of copying gigabytes only to hit ENOSPC. The original is
        // never touched on failure.
        let target_dev = fs::metadata(&self.target).map(|m| m.dev()).unwrap_or(0);
        if meta.dev() != target_dev {
            let need = dir_size(path);
            if let Some(avail) = available_space(&self.target)
                && need > avail
            {
                return Err(format!(
                    "cannot remove {}: not enough space in trash target (need {}, {} free)",
                    quote_name(disp),
                    human_size(need),
                    human_size(avail)
                ));
            }
        }

        // Pick a fresh name. UUIDv7 collisions are astronomically unlikely, but
        // we still must NEVER overwrite an existing entry (a crash could also
        // leave an orphan blob), so regenerate until the slot is free.
        let mut id = Uuid::now_v7().to_string();
        let mut dest = self.target.join(&id);
        let mut tries = 0;
        while fs::symlink_metadata(&dest).is_ok() {
            if tries >= 1000 {
                return Err(format!(
                    "cannot remove {}: could not allocate a unique trash name",
                    quote_name(disp)
                ));
            }
            tries += 1;
            id = Uuid::now_v7().to_string();
            dest = self.target.join(&id);
        }

        // 1) Record the entry FIRST (with its full path/metadata). If anything
        //    later fails, we withdraw this row. But the path info is never lost
        //    to an info-less orphan, and the move never precedes its record.
        let entry = db::TrashEntry {
            seq: 0, // assigned by the database
            id: id.clone(),
            original_path: original,
            deleted_at: now_micros(),
            original_dev: meta.dev() as i64,
            is_dir: meta.is_dir(),
            caller: self.caller.clone(),
            caller_cmdline: caller_cmdline().unwrap_or_default(),
            compressed: false,
        };
        db::insert(&self.conn, &entry)
            .map_err(|e| format!("cannot record {} in trash: {e}", quote_name(disp)))?;

        // 2) Move into the trash (rename, or copy+remove across filesystems).
        //    On failure the source is untouched, so withdraw the row and clean
        //    up any partial copy.
        if let Err(e) = move_path(path, &dest) {
            let _ = remove_recursive(&dest);
            let _ = db::remove(&self.conn, &id);
            return Err(format!("cannot remove {}: {e}", quote_name(disp)));
        }
        Ok(())
    }

    /// --one-file-system: trash only the same-device parts of `path` into one
    /// entry, leaving mounts (and the directories leading to them) in place.
    fn trash_partial(&self, path: &Path, disp: &[u8]) -> Result<(), String> {
        let root_dev = dev_of(path);
        let original = original_abs(path).to_string_lossy().to_string();

        let mut id = Uuid::now_v7().to_string();
        let mut dest = self.target.join(&id);
        let mut tries = 0;
        while fs::symlink_metadata(&dest).is_ok() {
            if tries >= 1000 {
                return Err(format!(
                    "cannot remove {}: could not allocate a unique trash name",
                    quote_name(disp)
                ));
            }
            tries += 1;
            id = Uuid::now_v7().to_string();
            dest = self.target.join(&id);
        }

        // Record FIRST so the entry (with its path) is tracked. A partial move
        // relocates entries one by one (source emptied as we go), so on failure
        // we must NOT delete the destination. The already-moved data stays
        // under this still-present row and is recoverable via its id.
        let entry = db::TrashEntry {
            seq: 0,
            id,
            original_path: original,
            deleted_at: now_micros(),
            original_dev: root_dev as i64,
            is_dir: true,
            caller: self.caller.clone(),
            caller_cmdline: caller_cmdline().unwrap_or_default(),
            compressed: false,
        };
        db::insert(&self.conn, &entry)
            .map_err(|e| format!("cannot record {} in trash: {e}", quote_name(disp)))?;

        if let Err(e) = move_same_device(path, &dest, root_dev) {
            return Err(format!(
                "cannot remove {}: {e} (moved parts are tracked in the trash and recoverable)",
                quote_name(disp)
            ));
        }
        Ok(())
    }
}

/// Microseconds since the Unix epoch (saturating, never panics).
fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Absolute original location of `path`, computed before it is moved. The
/// final component is kept literal (we never resolve a symlink operand itself).
fn original_abs(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => match parent.canonicalize()
        {
            Ok(cp) => cp.join(name),
            Err(_) => absolutize(path),
        },
        _ => absolutize(path),
    }
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(path)
    }
}

/// Move `src` to `dest`: a same-filesystem rename (instant, atomic) when
/// possible, otherwise a recursive copy followed by removal of the source.
fn move_path(src: &Path, dest: &Path) -> Result<(), String> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        // EXDEV (18): rename cannot cross filesystems -> copy then delete.
        Err(e) if e.raw_os_error() == Some(18) => {
            copy_recursive(src, dest).map_err(|e| os_err(&e))?;
            remove_recursive(src).map_err(|e| os_err(&e))?;
            Ok(())
        }
        Err(e) => Err(os_err(&e)),
    }
}

/// Recursively copy `src` to `dest`, preserving metadata (permissions, owner,
/// and access/modification times) for every node. Used only when moving across
/// filesystems. A same-filesystem rename keeps all metadata for free.
///
/// Note: ctime (inode-change time) is kernel-managed and cannot be restored;
/// this is a hard limitation of any copy, GNU `cp -p` included.
fn copy_recursive(src: &Path, dest: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let link_target = fs::read_link(src)?;
        std::os::unix::fs::symlink(link_target, dest)?;
        preserve_owner(dest, &meta, true);
        preserve_times(dest, &meta, true);
    } else if ft.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dest.join(entry.file_name()))?;
        }
        // Apply directory metadata AFTER its children exist, so populating the
        // directory does not bump the mtime we just restored.
        let _ = fs::set_permissions(dest, meta.permissions());
        preserve_owner(dest, &meta, false);
        preserve_times(dest, &meta, false);
    } else {
        fs::copy(src, dest)?; // copies contents and permission bits
        preserve_owner(dest, &meta, false);
        preserve_times(dest, &meta, false);
    }
    Ok(())
}

/// Best-effort restore of ownership (no-op when unprivileged and unchanged,
/// exactly like `cp -p`).
fn preserve_owner(path: &Path, meta: &Metadata, symlink: bool) {
    if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
        unsafe {
            if symlink {
                lchown(c.as_ptr(), meta.uid(), meta.gid());
            } else {
                chown(c.as_ptr(), meta.uid(), meta.gid());
            }
        }
    }
}

/// Restore access and modification times with nanosecond precision.
fn preserve_times(path: &Path, meta: &Metadata, symlink: bool) {
    let times = [
        Timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() },
        Timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() },
    ];
    if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
        let flags = if symlink { AT_SYMLINK_NOFOLLOW } else { 0 };
        unsafe {
            utimensat(AT_FDCWD, c.as_ptr(), times.as_ptr(), flags);
        }
    }
}

fn remove_recursive(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// The device id of `path` (0 if it cannot be stat'd).
fn dev_of(path: &Path) -> u64 {
    fs::symlink_metadata(path).map(|m| m.dev()).unwrap_or(0)
}

/// True if any directory below `dir` lives on a different filesystem than
/// `root_dev` (i.e. there is a mountpoint to skip under --one-file-system).
fn subtree_crosses_device(dir: &Path, root_dev: u64) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if m.is_dir()
            && !m.file_type().is_symlink()
            && (m.dev() != root_dev || subtree_crosses_device(&p, root_dev))
        {
            return true;
        }
    }
    false
}

/// --one-file-system permanent delete: recursively remove `dir`'s contents but
/// leave any subdirectory on a different filesystem (and the dirs leading to
/// it). Returns whether `dir` ended up empty (caller may rmdir it).
fn remove_same_device(dir: &Path, root_dev: u64) -> io::Result<bool> {
    let mut emptied = true;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let m = fs::symlink_metadata(&p)?;
        if m.is_dir() && !m.file_type().is_symlink() {
            if m.dev() != root_dev {
                emptied = false; // mountpoint: leave it
                continue;
            }
            if remove_same_device(&p, root_dev)? {
                fs::remove_dir(&p)?;
            } else {
                emptied = false;
            }
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(emptied)
}

/// --one-file-system trash: move `src`'s same-device contents into the mirror
/// directory `dest`, skipping (leaving) any subdirectory on a different
/// filesystem. Returns whether `src` ended up empty.
fn move_same_device(src: &Path, dest: &Path, root_dev: u64) -> Result<bool, String> {
    fs::create_dir_all(dest).map_err(|e| format!("cannot create '{}': {}", dest.display(), os_err(&e)))?;
    let mut emptied = true;
    let rd = fs::read_dir(src).map_err(|e| format!("cannot read '{}': {}", src.display(), os_err(&e)))?;
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        let ddest = dest.join(entry.file_name());
        if m.is_dir() && !m.file_type().is_symlink() {
            if m.dev() != root_dev {
                emptied = false; // mountpoint: leave it
                continue;
            }
            if subtree_crosses_device(&p, root_dev) {
                // Contains a deeper mount -> recurse partially.
                move_same_device(&p, &ddest, root_dev)?;
                if fs::read_dir(&p).map(|mut i| i.next().is_none()).unwrap_or(false) {
                    let _ = fs::remove_dir(&p);
                } else {
                    emptied = false;
                }
            } else {
                move_path(&p, &ddest)?; // fully same-device -> wholesale
            }
        } else {
            move_path(&p, &ddest)?;
        }
    }
    // Mirror the source directory's own metadata onto dest.
    if let Ok(m) = fs::symlink_metadata(src) {
        let _ = fs::set_permissions(dest, m.permissions());
        preserve_owner(dest, &m, false);
        preserve_times(dest, &m, false);
    }
    Ok(emptied)
}

/// Permanently delete the same-device parts of a tree (--one-file-system),
/// leaving mounts in place.
fn permanent_delete_partial(path: &Path, disp: &[u8]) -> Result<(), String> {
    let root_dev = dev_of(path);
    remove_same_device(path, root_dev)
        .map_err(|e| format!("cannot remove {}: {}", quote_name(disp), os_err(&e)))?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

/// True if the last component of the (verbatim) path is "." or "..", ignoring
/// trailing slashes. Matches GNU's last_component()/dot_or_dotdot() check.
fn last_is_dot(disp: &[u8]) -> bool {
    // Strip trailing '/', then take the bytes after the last '/'.
    let trimmed = match disp.iter().rposition(|&b| b != b'/') {
        Some(i) => &disp[..=i],
        None => disp,
    };
    let last = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    last == b"." || last == b".."
}

fn dir_has_entries(path: &Path) -> bool {
    fs::read_dir(path).map(|mut it| it.next().is_some()).unwrap_or(true)
}

fn is_root_path(path: &Path) -> bool {
    match path.canonicalize() {
        Ok(p) => p == Path::new("/"),
        Err(_) => path == Path::new("/"),
    }
}

/// Quote a (possibly non-UTF-8) file name exactly like GNU coreutils'
/// shell-escape style, used in all diagnostics:
///   plain            -> 'name'
///   contains '       -> "it's"          (double-quote style, no control bytes)
///   control/8-bit    -> 'a'$'\n''b'     (single-quote runs glued to $'...' runs)
fn quote_name(bytes: &[u8]) -> String {
    let needs_dollar = bytes.iter().any(|&b| b < 0x20 || b == 0x7f || b >= 0x80);
    let has_squote = bytes.contains(&b'\'');

    // Simple case: ordinary name, just single-quote it.
    if !needs_dollar && !has_squote {
        let mut s = String::with_capacity(bytes.len() + 2);
        s.push('\'');
        s.push_str(&String::from_utf8_lossy(bytes));
        s.push('\'');
        return s;
    }

    // Has a single quote but no control bytes: GNU uses double quotes.
    if !needs_dollar {
        let mut s = String::from("\"");
        for &b in bytes {
            if matches!(b, b'"' | b'$' | b'`' | b'\\') {
                s.push('\\');
            }
            s.push(b as char);
        }
        s.push('"');
        return s;
    }

    // Control/8-bit bytes present: alternate '...' literal runs with $'...'
    // escape runs. A literal ' becomes \' between runs.
    let mut s = String::new();
    let mut lit_open = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' {
            if lit_open {
                s.push('\'');
                lit_open = false;
            }
            s.push_str("\\'");
            i += 1;
        } else if b < 0x20 || b == 0x7f || b >= 0x80 {
            // A $'...' run must be preceded by a literal run (empty if needed).
            if lit_open {
                s.push('\'');
                lit_open = false;
            } else {
                s.push_str("''");
            }
            s.push_str("$'");
            while i < bytes.len() {
                let c = bytes[i];
                if c >= 0x20 && c != 0x7f && c < 0x80 {
                    break;
                }
                s.push_str(&escape_byte(c));
                i += 1;
            }
            s.push('\'');
        } else {
            if !lit_open {
                s.push('\'');
                lit_open = true;
            }
            s.push(b as char);
            i += 1;
        }
    }
    if lit_open {
        s.push('\'');
    }
    s
}

/// One byte inside a $'...' run: a named C escape where GNU uses one, else octal.
fn escape_byte(b: u8) -> String {
    match b {
        0x07 => "\\a".into(),
        0x08 => "\\b".into(),
        0x09 => "\\t".into(),
        0x0a => "\\n".into(),
        0x0b => "\\v".into(),
        0x0c => "\\f".into(),
        0x0d => "\\r".into(),
        _ => format!("\\{b:03o}"),
    }
}

/// Reproduce GNU's error wording by stripping Rust's " (os error N)" suffix,
/// leaving the libc strerror() text that GNU prints verbatim.
fn os_err(e: &io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s.get(..i).map(|p| p.to_string()).unwrap_or(s.clone()),
        None => s,
    }
}

/// The program that invoked rm: the parent process's `comm` (e.g. "make",
/// "bash"). None if it cannot be determined.
fn caller_comm() -> Option<String> {
    let ppid = unsafe { getppid() };
    let comm = fs::read_to_string(format!("/proc/{ppid}/comm")).ok()?;
    let trimmed = comm.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The parent process's command line as individual arguments (NUL-separated in
/// /proc). Empty if unavailable.
fn caller_cmdline_args() -> Vec<String> {
    let ppid = unsafe { getppid() };
    match fs::read(format!("/proc/{ppid}/cmdline")) {
        Ok(raw) => raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The full command line of the parent process, rejoined with spaces.
fn caller_cmdline() -> Option<String> {
    let parts = caller_cmdline_args();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Names a blacklist entry may match: the program (`comm`), plus each command
/// line argument and its basename. So scripts like `deploy.sh` / `app.py`
/// (run via a shell/interpreter) can be blacklisted too.
fn caller_candidates(comm: &str) -> Vec<String> {
    let mut names = Vec::new();
    if !comm.is_empty() {
        names.push(comm.to_string());
    }
    for arg in caller_cmdline_args() {
        if let Some(base) = Path::new(&arg).file_name() {
            names.push(base.to_string_lossy().into_owned());
        }
        names.push(arg);
    }
    names
}

/// Bytes currently available to an unprivileged process at `path`'s filesystem.
/// None if it cannot be determined (then callers skip the pre-flight check).
fn available_space(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s = Statvfs::zeroed();
    if unsafe { statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

/// True if the effective user cannot write the path (root can always write).
fn is_write_protected(path: &Path) -> bool {
    if unsafe { geteuid() } == 0 {
        return false;
    }
    match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => unsafe { access(c.as_ptr(), 2 /* W_OK */) != 0 },
        Err(_) => false,
    }
}

/// POSIX `struct timespec` (64-bit `time_t`/`long` on the targeted platforms).
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
const SIGPIPE: i32 = 13;
const SIG_DFL: usize = 0;

/// glibc `struct tm` (9 ints followed by tm_gmtoff/tm_zone on Linux).
#[repr(C)]
struct Tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const c_char,
}

impl Tm {
    fn zeroed() -> Self {
        Self {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
        }
    }
}

/// glibc `struct statvfs` (LP64 layout). Only f_frsize/f_bavail are read.
#[repr(C)]
struct Statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    f_spare: [u32; 6],
}

impl Statvfs {
    fn zeroed() -> Self {
        Self {
            f_bsize: 0,
            f_frsize: 0,
            f_blocks: 0,
            f_bfree: 0,
            f_bavail: 0,
            f_files: 0,
            f_ffree: 0,
            f_favail: 0,
            f_fsid: 0,
            f_flag: 0,
            f_namemax: 0,
            f_spare: [0; 6],
        }
    }
}

unsafe extern "C" {
    fn geteuid() -> u32;
    fn getppid() -> i32;
    fn signal(signum: i32, handler: usize) -> usize;
    fn statvfs(path: *const c_char, buf: *mut Statvfs) -> i32;
    fn access(path: *const c_char, mode: i32) -> i32;
    fn utimensat(dirfd: i32, path: *const c_char, times: *const Timespec, flags: i32) -> i32;
    fn chown(path: *const c_char, owner: u32, group: u32) -> i32;
    fn lchown(path: *const c_char, owner: u32, group: u32) -> i32;
    fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
    fn strftime(s: *mut c_char, max: usize, format: *const c_char, tm: *const Tm) -> usize;
}

/// GNU's yesno()/rpmatch: a reply is affirmative iff it begins with 'y'/'Y'.
fn prompt(msg: &str) -> bool {
    eprint!("{msg}"); // GNU rm writes prompts to stderr
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        return false; // EOF counts as "no"
    }
    matches!(line.as_bytes().first(), Some(b'y') | Some(b'Y'))
}

const HELP: &str = "\
Usage: rm [OPTION]... [FILE]...
A safe, drop-in rm: command-line compatible with GNU rm.

  -f, --force           ignore nonexistent files and arguments, never prompt
  -i                    prompt before every removal
  -I                    prompt once before removing more than three files, or
                          when removing recursively
      --interactive[=WHEN]  prompt according to WHEN: never, once (-I), or
                          always (-i). Without WHEN, prompt always
      --one-file-system  when removing a hierarchy recursively, skip any
                          directory on a different file system
      --no-preserve-root  do not treat '/' specially
      --preserve-root[=all]  do not act on '/' (default)
  -r, -R, --recursive   remove directories and their contents recursively
  -d, --dir             remove empty directories
  -v, --verbose         explain what is being done
      --help            display this help and exit
      --version         output version information and exit

By default, rm does not remove directories.  Use the --recursive (-r or -R)
option to remove directories and their contents.

Removed files are not destroyed: they go to a recoverable trash. Manage it
with the --trash subcommands:

      --trash on | off       enable/disable trashing (off = delete permanently)
      --trash list | ps      list trashed entries (ps also shows the caller)
      --trash restore <id>   restore entries to their original path ('all' too)
      --trash delete  <id>   permanently delete entries ('all' too)
      --trash clear          permanently empty the whole trash
      --trash size           show entry count and disk usage
      --trash prune          delete entries past the retention period
      --trash compress [id] | decompress [id]   pack/unpack entries (tar.zst)
      --trash orphans [clear|recover NAME DEST] manage blobs without a record
      --trash target [PATH]      show or set the storage location
      --trash retention [days]   show or set auto-prune age (default 30)
      --trash max-size [SIZE]    show or set a soft size-warning threshold
      --trash blacklist [add|rm PROG]   programs/scripts that delete permanently
      --trash whitelist [add|rm PATH]   trash only under these paths

Run 'rm --trash help' for details on the trash subcommands.
";

const VERSION: &str = concat!("rm (recycle-rm) ", env!("CARGO_PKG_VERSION"), "\n");

#[cfg(test)]
mod tests {
    use super::quote_name;

    #[test]
    fn quote_name_matches_gnu_shell_escape() {
        // Captured byte-for-byte from GNU coreutils rm 9.4.
        assert_eq!(quote_name(b"file.txt"), "'file.txt'");
        assert_eq!(quote_name(b"a b.txt"), "'a b.txt'");
        assert_eq!(quote_name(b"a\\b"), "'a\\b'");
        assert_eq!(quote_name(b"say \"hi\""), "'say \"hi\"'");
        assert_eq!(quote_name(b"it's.txt"), "\"it's.txt\"");
        assert_eq!(quote_name(b"a\nb.txt"), "'a'$'\\n''b.txt'");
        assert_eq!(quote_name(b"a\tb"), "'a'$'\\t''b'");
        assert_eq!(quote_name(b"x\n\ny"), "'x'$'\\n\\n''y'");
        assert_eq!(quote_name(b"\nlead"), "''$'\\n''lead'");
        assert_eq!(quote_name(b"it's\nbad"), "'it'\\''s'$'\\n''bad'");
        assert_eq!(quote_name(b"bad\xffname"), "'bad'$'\\377''name'");
    }
}
