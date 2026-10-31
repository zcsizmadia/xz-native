//! The `xz-native` binary: output byte-identical to xz 5.8.4 for the same
//! options, interoperability, and xz's file handling and exit statuses.
//! The comparisons need `xz` on the PATH; they are skipped without it
//! unless `XZ_NATIVE_REQUIRE_XZ` is set.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xz-native"));
    // The tests set every option themselves.
    c.env_remove("XZ_DEFAULTS").env_remove("XZ_OPT");
    c
}

/// Runs `cmd` with `input` on standard input.
fn run(mut cmd: Command, input: &[u8]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let t = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    t.join().unwrap();
    out
}

fn ours(args: &[&str], input: &[u8]) -> Output {
    let mut c = bin();
    c.args(args);
    run(c, input)
}

/// The system xz, or None if it isn't installed.
fn xz(args: &[&str], input: &[u8]) -> Option<Output> {
    if Command::new("xz").arg("--version").output().is_err() {
        assert!(
            std::env::var_os("XZ_NATIVE_REQUIRE_XZ").is_none(),
            "xz not found"
        );
        return None;
    }
    let mut c = Command::new("xz");
    c.args(args).env_remove("XZ_DEFAULTS").env_remove("XZ_OPT");
    Some(run(c, input))
}

fn status(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn sample(n: usize, seed: u32) -> Vec<u8> {
    const WORDS: [&str; 10] = [
        "stream ", "block ", "index ", "lzma ", "filter ", "check ", "the ", "a ", "\n", "xz ",
    ];
    let mut x = seed | 1;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v.extend_from_slice(WORDS[(x % 10) as usize].as_bytes());
        if x.is_multiple_of(7) {
            v.push((x >> 8) as u8);
        }
    }
    v.truncate(n);
    v
}

/// Compresses with both tools and compares the bytes and the status.
#[track_caller]
fn same_as_xz(args: &[&str], data: &[u8]) {
    let a = ours(args, data);
    let Some(b) = xz(args, data) else { return };
    assert_eq!(
        status(&a),
        status(&b),
        "{args:?}: status; ours: {}",
        stderr(&a)
    );
    assert!(
        a.stdout == b.stdout,
        "{args:?}: output differs ({} vs {} bytes)",
        a.stdout.len(),
        b.stdout.len()
    );
}

#[test]
fn compression_matches_xz() {
    let data = sample(3 << 20, 1);
    for args in [
        &["-c", "-T1", "-0"][..],
        &["-c", "-T1", "-6"],
        &["-c", "-T1", "-9e"],
        &["-c", "-T1", "-3", "--check=sha256"],
        &["-c", "-T1", "-1", "-C", "none", "--block-size=700KiB"],
        // Multithreaded mode, the default.
        &["-c", "-6"],
        &["-c", "-T3", "--block-size=1MiB"],
        &["-c", "-T+1", "-2"],
        &["-c", "-T0", "-1", "-C", "crc32"],
        // .lzma
        &["-c", "--format=lzma", "-7"],
        &["-c", "-F", "lzma", "-0e"],
        // Adjusting to --memlimit-compress: single-threaded mode, then a
        // smaller dictionary.
        &["-c", "-T1", "-9", "--memlimit-compress=200MiB"],
        &["-c", "-T2", "-8", "--memlimit-compress=300MiB"],
        &[
            "-c",
            "-T1",
            "-9",
            "--memlimit-compress=10MiB",
            "--no-adjust",
        ],
    ] {
        same_as_xz(args, &data);
    }
}

#[test]
fn decompression_interop() {
    let data = sample(1 << 20, 2);
    // xz's output, ours decoded, and the other way round.
    for args in [
        &["-c", "-6"][..],
        &["-c", "--format=lzma", "-2"],
        &["-c", "-T1", "-1", "--block-size=100000"],
    ] {
        let Some(theirs) = xz(args, &data) else {
            return;
        };
        let d = ours(&["-dc"], &theirs.stdout);
        assert_eq!(status(&d), 0, "{}", stderr(&d));
        assert!(d.stdout == data);
        let t = ours(&["-t"], &theirs.stdout);
        assert_eq!(status(&t), 0);
        assert!(t.stdout.is_empty());
        let mine = ours(args, &data);
        let back = xz(&["-dc"], &mine.stdout).unwrap();
        assert_eq!(status(&back), 0);
        assert!(back.stdout == data);
    }
}

#[test]
fn concatenated_and_single_stream() {
    let (a, b) = (sample(10_000, 3), sample(20_000, 4));
    let mut xz2 = ours(&["-c"], &a).stdout;
    xz2.extend_from_slice(&ours(&["-c", "-T1"], &b).stdout);
    let all = ours(&["-dc"], &xz2);
    assert!(all.stdout == [a.clone(), b].concat());
    let first = ours(&["-dc", "--single-stream"], &xz2);
    assert!(first.stdout == a);
}

#[test]
fn errors_on_bad_data() {
    let data = sample(100_000, 5);
    let mut xz = ours(&["-c", "-T1"], &data).stdout;
    let mid = xz.len() / 2;
    xz[mid] ^= 0x55;
    let o = ours(&["-t"], &xz);
    assert_eq!(status(&o), 1);
    assert!(
        stderr(&o).contains("(stdin): Compressed data is corrupt"),
        "{}",
        stderr(&o)
    );
    let o = ours(&["-t"], &xz[..mid]);
    assert_eq!(status(&o), 1);
    assert!(stderr(&o).contains("Unexpected end of input"));
    let o = ours(&["-d"], b"not compressed");
    assert_eq!(status(&o), 1);
    assert!(stderr(&o).contains("File format not recognized"));
    // -dcf copies unknown input as is, like cat.
    let o = ours(&["-dcf"], b"plain text");
    assert_eq!((status(&o), &o.stdout[..]), (0, &b"plain text"[..]));
    // Memory limit.
    let big = ours(&["-c", "-T1", "-9"], &data).stdout;
    let o = ours(&["-d", "--memlimit-decompress=1MiB"], &big);
    assert_eq!(status(&o), 1);
    assert!(
        stderr(&o).contains("Memory usage limit reached"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("The limit is 1 MiB."), "{}", stderr(&o));
}

#[test]
fn usage_errors() {
    for args in [
        &["--nope"][..],
        &["-T", "x"],
        &["--check=crc16"],
        &["-F", "raw"],
        &["--list"],
        &["-Fxz", "-S", ""],
    ] {
        let o = ours(args, b"");
        assert_eq!(status(&o), 1, "{args:?}");
    }
    let o = ours(&["--version"], b"");
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("xz-native "));
    let o = ours(&["-h"], b"");
    assert_eq!(status(&o), 0);
    // XZ_OPT supplies options but no files.
    let o = run(
        {
            let mut c = bin();
            c.env("XZ_OPT", "-0 -T1").arg("-c");
            c
        },
        b"abc",
    );
    assert!(o.stdout == ours(&["-c", "-0", "-T1"], b"abc").stdout);
}

/// A fresh directory under the target directory.
fn tmpdir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn file_cmd(dir: &Path, args: &[&str]) -> Output {
    let mut c = bin();
    c.current_dir(dir).args(args);
    run(c, b"")
}

#[test]
fn files() {
    let dir = tmpdir("files");
    let data = sample(50_000, 6);
    fs::write(dir.join("a.txt"), &data).unwrap();
    let mtime = fs::metadata(dir.join("a.txt")).unwrap().modified().unwrap();

    // Compress: a.txt -> a.txt.xz, the input removed, the time kept.
    let o = file_cmd(&dir, &["-T1", "a.txt"]);
    assert_eq!(status(&o), 0, "{}", stderr(&o));
    assert!(!dir.join("a.txt").exists());
    let meta = fs::metadata(dir.join("a.txt.xz")).unwrap();
    assert_eq!(meta.modified().unwrap(), mtime);

    // Already has the suffix: a warning, status 2.
    let o = file_cmd(&dir, &["a.txt.xz"]);
    assert_eq!(status(&o), 2);
    assert!(stderr(&o).contains("a.txt.xz: File already has '.xz' suffix, skipping"));

    // Decompress with -k: the input stays.
    let o = file_cmd(&dir, &["-dk", "a.txt.xz"]);
    assert_eq!(status(&o), 0, "{}", stderr(&o));
    assert!(fs::read(dir.join("a.txt")).unwrap() == data);
    assert!(dir.join("a.txt.xz").exists());

    // The output exists: an error, nothing changed; -f replaces it.
    let o = file_cmd(&dir, &["-d", "a.txt.xz"]);
    assert_eq!(status(&o), 1);
    assert!(dir.join("a.txt.xz").exists());
    let o = file_cmd(&dir, &["-df", "a.txt.xz"]);
    assert_eq!(status(&o), 0, "{}", stderr(&o));
    assert!(!dir.join("a.txt.xz").exists());
    assert!(fs::read(dir.join("a.txt")).unwrap() == data);

    // Unknown suffix when decompressing; .txz becomes .tar; -S.
    fs::write(dir.join("b.bin"), b"x").unwrap();
    let o = file_cmd(&dir, &["-d", "b.bin"]);
    assert_eq!(status(&o), 2);
    assert!(stderr(&o).contains("b.bin: Filename has an unknown suffix, skipping"));
    fs::write(dir.join("c.txz"), ours(&["-c"], b"tar").stdout).unwrap();
    assert_eq!(status(&file_cmd(&dir, &["-d", "c.txz"])), 0);
    assert_eq!(fs::read(dir.join("c.tar")).unwrap(), b"tar");
    assert_eq!(status(&file_cmd(&dir, &["-S", ".pack", "c.tar"])), 0);
    assert!(dir.join("c.tar.pack").exists());
    assert_eq!(
        status(&file_cmd(&dir, &["-d", "-S", ".pack", "c.tar.pack"])),
        0
    );
    assert_eq!(fs::read(dir.join("c.tar")).unwrap(), b"tar");

    // .lzma names with --format=lzma.
    assert_eq!(
        status(&file_cmd(&dir, &["--format=lzma", "-k", "c.tar"])),
        0
    );
    assert!(dir.join("c.tar.lzma").exists());

    // A directory is skipped; a missing file is an error, and the error
    // wins over a warning.
    fs::create_dir(dir.join("sub")).unwrap();
    let o = file_cmd(&dir, &["sub"]);
    assert_eq!(status(&o), 2);
    assert!(stderr(&o).contains("sub: Is a directory, skipping"));
    let o = file_cmd(&dir, &["sub", "missing"]);
    assert_eq!(status(&o), 1);
    // -Q: warnings don't change the status.
    assert_eq!(status(&file_cmd(&dir, &["-Q", "sub"])), 0);

    // A corrupt file leaves no output behind and keeps the input.
    let mut bad = ours(&["-c"], &data).stdout;
    let n = bad.len();
    bad[n / 2] ^= 1;
    fs::write(dir.join("bad.xz"), &bad).unwrap();
    let o = file_cmd(&dir, &["-d", "bad.xz"]);
    assert_eq!(status(&o), 1);
    assert!(!dir.join("bad").exists());
    assert!(dir.join("bad.xz").exists());

    // -c and -t keep the input and write no files.
    let o = file_cmd(&dir, &["-c", "a.txt"]);
    assert_eq!(status(&o), 0);
    assert!(dir.join("a.txt").exists());
    let o = file_cmd(&dir, &["-t", "c.tar.lzma"]);
    assert_eq!(status(&o), 0);
    assert!(dir.join("c.tar.lzma").exists());
}

#[cfg(unix)]
#[test]
fn symlinks_and_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tmpdir("links");
    fs::write(dir.join("f"), b"data").unwrap();
    fs::set_permissions(dir.join("f"), fs::Permissions::from_mode(0o640)).unwrap();
    symlink("f", dir.join("l")).unwrap();
    let o = file_cmd(&dir, &["l"]);
    assert_eq!(status(&o), 2);
    assert!(stderr(&o).contains("l: Is a symbolic link, skipping"));
    assert_eq!(status(&file_cmd(&dir, &["f"])), 0);
    let mode = fs::metadata(dir.join("f.xz")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640);
}

/// Runs `cmd` in `dir` with the C locale (no thousands separators, except
/// in xz's Windows builds, which always group, as xz-native does there).
fn in_c_locale(mut cmd: Command, dir: &Path, args: &[&str]) -> Output {
    cmd.current_dir(dir).args(args).env("LC_ALL", "C");
    run(cmd, b"")
}

#[test]
fn list_matches_xz() {
    use xz_native::{BcjKind, Check, EncoderOptions, Filter};
    let dir = tmpdir("list");
    let data = sample(300_000, 7);
    let opts = |f: &dyn Fn(&mut EncoderOptions)| {
        let mut o = EncoderOptions::preset(1).unwrap();
        o.lzma.dict_size = 1 << 20;
        f(&mut o);
        xz_native::compress_with_options(&data, &o).unwrap()
    };
    // Several Blocks with sizes in the headers.
    fs::write(
        dir.join("a.xz"),
        opts(&|o| {
            o.threads = 3;
            o.block_size = 100_000;
        }),
    )
    .unwrap();
    fs::write(dir.join("b.xz"), opts(&|o| o.check = Check::Sha256)).unwrap();
    // Two Streams with padding, check None, and filters before LZMA2.
    let mut c = opts(&|o| o.check = Check::None);
    c.extend_from_slice(&[0; 8]);
    c.extend_from_slice(&opts(&|o| {
        o.check = Check::Crc32;
        o.lzma.dict_size = 6 << 10;
        o.filters = vec![
            Filter::Delta { distance: 4 },
            Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 0,
            },
        ];
    }));
    c.extend_from_slice(&[0; 4]);
    fs::write(dir.join("c.xz"), c).unwrap();
    fs::write(dir.join("e.xz"), xz_native::compress(&[], 6).unwrap()).unwrap();
    fs::write(dir.join("empty.xz"), b"").unwrap();
    fs::write(dir.join("small.xz"), b"tiny").unwrap();
    fs::write(dir.join("bad.xz"), [0u8; 64]).unwrap();

    for args in [
        &["-l"][..],
        &["-lv"],
        &["-lvv"],
        &["-l", "--robot"],
        &["-lv", "--robot"],
        &["-lvv", "--robot"],
        &["-lq"],
    ] {
        for files in [
            &["a.xz"][..],
            &["c.xz"],
            &["e.xz"],
            &["a.xz", "b.xz", "c.xz", "e.xz"],
            &["b.xz", "empty.xz", "small.xz", "bad.xz", "missing.xz"],
        ] {
            let all: Vec<&str> = args.iter().chain(files).copied().collect();
            let theirs = match Command::new("xz").arg("--version").output() {
                Ok(_) => in_c_locale(Command::new("xz"), &dir, &all),
                Err(_) => {
                    assert!(
                        std::env::var_os("XZ_NATIVE_REQUIRE_XZ").is_none(),
                        "xz not found"
                    );
                    return;
                }
            };
            let ours = in_c_locale(bin(), &dir, &all);
            let text = |b: &[u8]| String::from_utf8_lossy(b).replace('\r', "");
            assert_eq!(text(&ours.stdout), text(&theirs.stdout), "{all:?}: stdout");
            assert_eq!(
                text(&ours.stderr).replace("xz-native:", "xz:"),
                text(&theirs.stderr),
                "{all:?}: stderr"
            );
            assert_eq!(status(&ours), status(&theirs), "{all:?}: status");
        }
    }
    // Standard input and other formats.
    let o = ours(&["-l"], b"");
    assert_eq!(status(&o), 1);
    assert!(stderr(&o).contains("--list does not support reading from standard input"));
    let o = file_cmd(&dir, &["-l", "--format=lzma", "a.xz"]);
    assert_eq!(status(&o), 1);
    assert!(stderr(&o).contains("Try 'lzmainfo'"));
}
