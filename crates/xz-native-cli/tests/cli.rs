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
/// A check type xz can't verify: a warning per Stream and decoding goes
/// on, as in xz; none with `--ignore-check`; `-Q` keeps the status 0.
#[test]
fn unsupported_check_warns_like_xz() {
    let dir = tmpdir("unsupported-check");
    let file = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/xz/tests/files/unsupported-check.xz"),
    )
    .unwrap();
    fs::write(dir.join("u.xz"), &file).unwrap();
    fs::write(dir.join("u2.xz"), [file.clone(), file].concat()).unwrap();
    for args in [
        &["-dc", "u2.xz"][..],
        &["-dc", "-T1", "u2.xz"],
        &["-tv", "u.xz"],
        &["-dcQ", "u.xz"],
        &["-dcq", "u.xz"],
        &["-dc", "--ignore-check", "u.xz"],
    ] {
        let o = in_c_locale(bin(), &dir, args);
        let warnings = stderr(&o)
            .matches("Unsupported type of integrity check")
            .count();
        let want = match args {
            [_, "u2.xz"] | [_, _, "u2.xz"] => 2,
            [_, "--ignore-check", _] | ["-dcq", _] => 0,
            _ => 1,
        };
        assert_eq!(warnings, want, "{args:?}: {}", stderr(&o));
        let theirs = match Command::new("xz").arg("--version").output() {
            Ok(_) => in_c_locale(Command::new("xz"), &dir, args),
            Err(_) => continue,
        };
        assert_eq!(
            stderr(&o).replace("xz-native:", "xz:"),
            stderr(&theirs).replace('\r', ""),
            "{args:?}"
        );
        assert_eq!(status(&o), status(&theirs), "{args:?}");
        assert!(o.stdout == theirs.stdout, "{args:?}");
    }
}

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

#[test]
fn filter_chains_match_xz() {
    let data = sample(1 << 20, 8);
    for args in [
        &["-c", "-T1", "--x86", "--lzma2=preset=1,dict=1MiB"][..],
        &["-c", "--x86", "--lzma2=preset=1,dict=1MiB"],
        &["-c", "-T1", "--delta=dist=4", "--lzma2=lc=0,lp=2,pb=0"],
        &["-c", "-T1", "--arm64=start=4096", "--lzma2=preset=3e"],
        &["-c", "-T1", "--filters=arm64 lzma2:preset=3e"],
        &[
            "-c",
            "-T2",
            "--filters=--x86 --delta=dist=2 --lzma2=dict=1MiB,mf=hc4,mode=fast,nice=64,depth=10",
        ],
        &["-c", "-T1", "--filters=2e"],
        // A preset or -e after the filters replaces them, and filters after
        // a preset replace it.
        &["-c", "-T1", "-6", "--x86"],
        &["-c", "-T1", "--x86", "-9"],
        &["-c", "-T1", "--x86", "-e"],
        &["-c", "-T1", "--format=lzma", "--lzma1=preset=4,mf=hc4"],
    ] {
        same_as_xz(args, &data);
    }
    // Errors: the same status and messages.
    for args in [
        &["-c", "--format=lzma", "--lzma2"][..],
        &["-c", "-T1", "--lzma1"],
        &["-c", "-T1", "--lzma2", "--x86"],
        &["-c", "-T1", "--x86", "--x86", "--x86", "--x86", "--lzma2"],
        &["-c", "-T1", "--lzma2=foo=1"],
        &["-c", "-T1", "--lzma2=dict=1KiB"],
        &["-c", "-T1", "--lzma2=lc=3,lp=2"],
        &["-c", "-T1", "--delta=dist=0"],
        &["-c", "-T1", "--filters=x86"],
        &["-c", "-T1", "--filters=x86 lzma2:dict=1KiB"],
        &["-c", "-T1", "--filters=bogus"],
    ] {
        let a = ours(args, b"data");
        let Some(b) = xz(args, b"data") else { return };
        assert_eq!(status(&a), status(&b), "{args:?}");
        assert_eq!(
            stderr(&a).replace("xz-native:", "xz:"),
            stderr(&b).replace('\r', ""),
            "{args:?}"
        );
    }
}

#[test]
fn block_lists_match_xz() {
    let data = sample(2 << 20, 9);
    for args in [
        &["-c", "-T1", "--block-list=100000,200000,300000"][..],
        &["-c", "-T1", "--block-list=100000,200000,0"],
        &["-c", "-T1", "--block-list=100000,,"],
        &[
            "-c",
            "-T1",
            "--block-list=150000,700000",
            "--block-size=300000",
        ],
        &["-c", "--block-list=100000,200000,300000"],
        &["-c", "-T3", "--block-list=1MiB,500KiB"],
        &["-c", "-T3", "--block-list=100000", "--block-size=50000"],
        // Filter chains per Block, with update_filters between them.
        &[
            "-c",
            "-T1",
            "--filters1=x86 lzma2:preset=1",
            "--block-list=1:100000,200000,1:50000,0",
        ],
        &[
            "-c",
            "-T1",
            "--filters1=delta:dist=4 lzma2:dict=1MiB,lc=0",
            "--filters2=arm64 lzma2:preset=0",
            "--block-list=2:70000,1:90000,100000",
        ],
        &[
            "-c",
            "--filters1=lzma2:preset=1",
            "--filters2=x86 lzma2:dict=2MiB",
            "--block-list=1:500000,2:300000,1:1MiB,0",
        ],
        &["-c", "-T2", "--filters3=6e", "--block-list=3:400000,800000"],
        // Every chain over the memory limit gets a smaller dictionary.
        &[
            "-c",
            "-T1",
            "-9",
            "--memlimit-compress=150MiB",
            "--filters1=lzma2:preset=8",
            "--block-list=1:500000,0",
        ],
    ] {
        same_as_xz(args, &data);
    }
    for args in [
        &["-c", "-T1", "--block-list=1:100000"][..],
        &["-c", "-T1", "--block-list=1:"],
        &["-c", "-T1", "--block-list=,100"],
        &["-c", "-T1", "--block-list=0,100"],
        &["-c", "-T1", "--filters1=lzma1", "--block-list=1:100"],
        &["-c", "--format=lzma", "--block-list=100"],
    ] {
        let a = ours(args, b"data");
        let Some(b) = xz(args, b"data") else { return };
        assert_eq!(status(&a), status(&b), "{args:?}");
        assert_eq!(
            stderr(&a).replace("xz-native:", "xz:"),
            stderr(&b).replace('\r', ""),
            "{args:?}"
        );
    }
}

/// `-v` with standard error not a terminal: a line per file at the end,
/// as in xz (no speed and time for runs this short).
#[test]
fn verbose_matches_xz() {
    let dir = tmpdir("verbose");
    let data = sample(300_000, 10);
    fs::write(dir.join("a"), &data).unwrap();
    fs::write(dir.join("b"), b"x").unwrap();
    fs::write(dir.join("e"), b"").unwrap();
    fs::write(dir.join("a.xz"), ours(&["-c", "-T1"], &data).stdout).unwrap();
    for args in [
        &["-v", "-c", "-T1", "a", "b", "e"][..],
        &["-dv", "-c", "a.xz"],
        &["-tv", "a.xz", "missing"],
        &["-v", "-c", "--format=lzma", "a"],
    ] {
        let o = in_c_locale(bin(), &dir, args);
        assert!(stderr(&o).contains("a"), "{args:?}: {}", stderr(&o));
        let theirs = match Command::new("xz").arg("--version").output() {
            Ok(_) => in_c_locale(Command::new("xz"), &dir, args),
            Err(_) => continue,
        };
        assert_eq!(
            stderr(&o).replace("xz-native:", "xz:"),
            stderr(&theirs).replace('\r', ""),
            "{args:?}"
        );
        assert_eq!(status(&o), status(&theirs), "{args:?}");
    }
}

#[test]
fn raw_format_matches_xz() {
    let data = sample(1 << 20, 11);
    for args in [
        &["-c", "-F", "raw"][..],
        &["-c", "-F", "raw", "-3e"],
        &["-c", "-F", "raw", "--lzma2=preset=1,dict=1MiB"],
        &["-c", "-F", "raw", "--lzma1=preset=2"],
        &[
            "-c",
            "-F",
            "raw",
            "--x86",
            "--delta=dist=2",
            "--lzma2=preset=0",
        ],
        &["-c", "-F", "raw", "--filters=arm64 lzma1:lc=0,lp=2"],
        &["-c", "-F", "raw", "--lzma2", "--memlimit-compress=10MiB"],
        &["-c", "-F", "raw", "--x86"],
    ] {
        same_as_xz(args, &data);
        // The preset warning and the errors too.
        let a = ours(args, &data);
        let Some(b) = xz(args, &data) else { return };
        assert_eq!(
            stderr(&a).replace("xz-native:", "xz:"),
            stderr(&b).replace('\r', ""),
            "{args:?}"
        );
    }
    // Round trips, also through xz, and trailing data is an error.
    for chain in [
        &["--lzma2=preset=1"][..],
        &["--lzma1=preset=2"],
        &["--delta=dist=4", "--lzma2=preset=0"],
    ] {
        let enc: Vec<&str> = ["-c", "-F", "raw"].iter().chain(chain).copied().collect();
        let dec: Vec<&str> = ["-dc", "-F", "raw"].iter().chain(chain).copied().collect();
        let raw = ours(&enc, &data).stdout;
        assert!(ours(&dec, &raw).stdout == data, "{chain:?}");
        if let Some(theirs) = xz(&dec, &raw) {
            assert!(theirs.stdout == data, "{chain:?}");
        }
        let mut junk = raw.clone();
        junk.extend_from_slice(b"junk");
        let o = ours(&dec, &junk);
        assert_eq!(status(&o), 1);
        assert!(stderr(&o).contains("Compressed data is corrupt"));
    }
    // Files need --suffix; with it, names work as with other formats.
    let dir = tmpdir("raw");
    fs::write(dir.join("f"), &data).unwrap();
    let o = file_cmd(&dir, &["-F", "raw", "f"]);
    assert_eq!(status(&o), 1);
    assert!(stderr(&o).contains("--suffix=.SUF is required"));
    assert_eq!(status(&file_cmd(&dir, &["-F", "raw", "-S", ".r", "f"])), 0,);
    assert!(dir.join("f.r").exists());
    let o = file_cmd(&dir, &["-d", "-F", "raw", "-S", ".r", "f.r"]);
    assert_eq!(status(&o), 0, "{}", stderr(&o));
    assert!(fs::read(dir.join("f")).unwrap() == data);
}

#[test]
fn files_from_lists() {
    let dir = tmpdir("files-from");
    let reset = || {
        for f in ["a.xz", "b.xz", "-.xz"] {
            let _ = fs::remove_file(dir.join(f));
        }
    };
    fs::write(dir.join("a"), b"alpha").unwrap();
    fs::write(dir.join("b"), b"beta").unwrap();
    fs::write(dir.join("-"), b"dash").unwrap();
    fs::write(dir.join("list"), b"a\n\n\nb\n").unwrap();
    fs::write(dir.join("list0"), b"a\0b\0").unwrap();
    fs::write(dir.join("nonl"), b"a\nb").unwrap();
    fs::write(dir.join("zero"), b"a\0b\n").unwrap();
    fs::write(dir.join("dash"), b"-\n").unwrap();
    for args in [&["-k", "--files=list"][..], &["-k", "--files0=list0"]] {
        reset();
        let o = file_cmd(&dir, args);
        assert_eq!(status(&o), 0, "{args:?}: {}", stderr(&o));
        assert_eq!(
            xz_native::decompress(&fs::read(dir.join("a.xz")).unwrap()).unwrap(),
            b"alpha"
        );
        assert_eq!(
            xz_native::decompress(&fs::read(dir.join("b.xz")).unwrap()).unwrap(),
            b"beta"
        );
    }
    // Names from standard input; "-" there is a file, not standard input.
    reset();
    let mut c = bin();
    c.current_dir(&dir).args(["-k", "--files"]);
    assert_eq!(status(&run(c, b"a\n-\n")), 0);
    assert!(dir.join("a.xz").exists() && dir.join("-.xz").exists());
    // xz's errors.
    reset();
    let o = file_cmd(&dir, &["-k", "--files=nonl"]);
    assert_eq!(status(&o), 1);
    assert!(stderr(&o).contains("nonl: Unexpected end of input when reading filenames"));
    assert!(dir.join("a.xz").exists() && !dir.join("b.xz").exists());
    let o = file_cmd(&dir, &["-k", "--files=zero"]);
    assert!(stderr(&o).contains("maybe you meant to use '--files0'"));
    let o = file_cmd(&dir, &["--files=list", "--files0=list0"]);
    assert!(stderr(&o).contains("Only one file can be specified"));
    let mut c = bin();
    c.current_dir(&dir).args(["-kf", "--files", "-"]);
    let o = run(c, b"a\n");
    assert!(stderr(&o).contains("Cannot read data from standard input when reading filenames"));
    let o = file_cmd(&dir, &["-k", "--files=missing"]);
    assert_eq!(status(&o), 1);
}

/// Decompressed files get holes for blocks of zeros (on Unix, as in xz);
/// the content is the same either way.
#[test]
fn sparse_output() {
    let dir = tmpdir("sparse");
    let mut data = vec![0u8; 8 << 20];
    data[..5000].copy_from_slice(&sample(5000, 12));
    data.extend_from_slice(b"end");
    data.extend(std::iter::repeat_n(0u8, 3 << 20));
    let xz = ours(&["-c", "-T1", "-1"], &data).stdout;
    for (name, extra) in [("s", None), ("n", Some("--no-sparse"))] {
        fs::write(dir.join(format!("{name}.xz")), &xz).unwrap();
        let mut args = vec!["-d"];
        args.extend(extra);
        let file = format!("{name}.xz");
        args.push(&file);
        let o = file_cmd(&dir, &args);
        assert_eq!(status(&o), 0, "{}", stderr(&o));
        assert!(fs::read(dir.join(name)).unwrap() == data);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let used = |n: &str| fs::metadata(dir.join(n)).unwrap().blocks() * 512;
        // Some file systems (tmpfs on old kernels, some CI disks) have no
        // holes; then both are the same size.
        assert!(used("s") <= used("n"));
        if used("s") < used("n") {
            assert!(used("s") < data.len() as u64 / 2);
        }

        // Standard output too: `>`, `>>` (O_APPEND is cleared and put back
        // for each file), and `1<>`, which writes over the start of a file
        // and so gets no holes.
        // `-d` above removed the .xz files.
        fs::write(dir.join("s.xz"), &xz).unwrap();
        let sh = |script: &str| {
            let o = Command::new("sh")
                .current_dir(&dir)
                .args(["-c", script])
                .arg(env!("CARGO_BIN_EXE_xz-native"))
                .output()
                .unwrap();
            assert_eq!(status(&o), 0, "{script}: {}", stderr(&o));
        };
        sh("\"$0\" -dc s.xz > o");
        assert!(fs::read(dir.join("o")).unwrap() == data);
        assert!(used("o") <= used("n"));
        fs::write(dir.join("a"), b"head").unwrap();
        sh("\"$0\" -dc s.xz s.xz >> a && echo tail >> a");
        let mut want = b"head".to_vec();
        want.extend_from_slice(&data);
        want.extend_from_slice(&data);
        want.extend_from_slice(b"tail\n");
        assert!(fs::read(dir.join("a")).unwrap() == want);
        let mut over = vec![0xffu8; data.len() + 10];
        fs::write(dir.join("w"), &over).unwrap();
        sh("\"$0\" -dc s.xz 1<> w");
        over[..data.len()].copy_from_slice(&data);
        assert!(fs::read(dir.join("w")).unwrap() == over);
    }
}

/// Starts `xz-native` on a file large and slow enough to be signalled
/// while it works.
#[cfg(unix)]
fn slow_child(dir: &Path, args: &[&str]) -> std::process::Child {
    let mut r = 0x9e37_79b9_7f4a_7c15u64;
    let data: Vec<u8> = (0..64 << 20)
        .map(|_| {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r as u8
        })
        .collect();
    fs::write(dir.join("big"), data).unwrap();
    let mut c = bin();
    c.current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn signal(child: &std::process::Child, sig: &str) {
    // Long enough to be coding, well short of the end.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let ok = Command::new("kill")
        .args([&format!("-{sig}"), &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -{sig}");
}

/// Ctrl-C: the partial output is removed, the input kept, and the process
/// ends with SIGINT, as with xz.
#[cfg(unix)]
#[test]
fn sigint_removes_the_output() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tmpdir("sigint");
    let child = slow_child(&dir, &["-9e", "-T1", "big"]);
    signal(&child, "INT");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.signal(), Some(2), "{:?}", out.status);
    assert!(!dir.join("big.xz").exists());
    assert!(dir.join("big").exists());
}

/// SIGUSR1 prints the progress, even without -v.
#[cfg(unix)]
#[test]
fn sigusr1_prints_progress() {
    let dir = tmpdir("sigusr1");
    let child = slow_child(&dir, &["-c", "-9e", "-T1", "big"]);
    signal(&child, "USR1");
    // Let it get to the next chunk, then stop it.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    signal(&child, "INT");
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("big (1/1)") && err.contains(" = "), "{err}");
}

#[test]
fn info_memory_and_help() {
    for args in [
        &["--info-memory"][..],
        &["-M", "50%", "--info-memory"],
        &[
            "--memlimit-decompress=1GiB",
            "--memlimit-mt-decompress=2GiB",
            "--info-memory",
        ],
        &["--robot", "--info-memory"],
        // It acts at once, so later options don't matter.
        &["--info-memory", "--bogus"],
    ] {
        let a = in_c_locale(bin(), Path::new("."), args);
        assert_eq!(status(&a), 0, "{args:?}");
        let Ok(_) = Command::new("xz").arg("--version").output() else {
            continue;
        };
        let b = in_c_locale(Command::new("xz"), Path::new("."), args);
        let text = |o: &Output| String::from_utf8_lossy(&o.stdout).replace('\r', "");
        // The RAM and thread count are the machine's; the layout and the
        // limits must agree.
        assert_eq!(text(&a), text(&b), "{args:?}");
    }
    let short = ours(&["-h"], b"");
    let long = ours(&["-H"], b"");
    let text = |o: &Output| String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(!text(&short).contains("--lzma2") && text(&long).contains("--lzma2"));
    assert!(text(&long).contains("--info-memory"));
}

/// The default thread budget stays 64 MiB below `ulimit -d`, as in xz.
/// Skipped where the shell can't lower the limit (macOS).
#[cfg(unix)]
#[test]
fn memory_budget_below_rlimit() {
    let out = Command::new("sh")
        .args([
            "-c",
            "ulimit -S -d 1048576 2>/dev/null || exit 77; exec \"$0\" --robot --info-memory",
        ])
        .arg(env!("CARGO_BIN_EXE_xz-native"))
        .output()
        .unwrap();
    if status(&out) == 77 {
        eprintln!("skipped: ulimit -d not supported");
        return;
    }
    assert_eq!(status(&out), 0, "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let fields: Vec<u64> = text
        .trim()
        .split('\t')
        .map(|f| f.parse().unwrap())
        .collect();
    assert_eq!(
        fields[4],
        (fields[0] / 4).min((1 << 30) - (64 << 20)),
        "{text}"
    );
}

/// `--flush-timeout`: after a pause in the input, what came so far is in
/// the output and decodes; the errors and the warning are xz's.
#[test]
fn flush_timeout() {
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    let first = sample(100_000, 13);
    let second = sample(50_000, 14);
    let mut child = bin()
        .args(["-c", "--flush-timeout=100"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let got = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let got = got.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    stdin.write_all(&first).unwrap();
    stdin.flush().unwrap();
    // Well past the timeout: the first part must be decodable already.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let so_far = got.lock().unwrap().clone();
    let mut dec = xz_native::XzDecoder::new();
    let mut out = vec![0u8; first.len() + 1000];
    let (_, n, _) = dec.decode(&so_far, &mut out, false).unwrap();
    assert!(
        out[..n] == first[..],
        "decoded {n} of {} bytes",
        first.len()
    );
    stdin.write_all(&second).unwrap();
    drop(stdin);
    let exit = child.wait().unwrap();
    reader.join().unwrap();
    assert!(exit.success());
    let all = got.lock().unwrap().clone();
    assert!(xz_native::decompress(&all).unwrap() == [first, second].concat());

    for args in [
        &["-c", "--flush-timeout=100"][..],
        &["-c", "-T1", "--flush-timeout=100", "--x86", "--lzma2"],
        &["-c", "-T1", "--flush-timeout=100", "-F", "lzma"],
        &[
            "-c",
            "-T1",
            "--flush-timeout=100",
            "--delta",
            "--lzma2=preset=1",
        ],
    ] {
        let a = ours(args, b"data");
        let Some(b) = xz(args, b"data") else { return };
        assert_eq!(status(&a), status(&b), "{args:?}");
        assert_eq!(
            stderr(&a).replace("xz-native:", "xz:"),
            stderr(&b).replace('\r', ""),
            "{args:?}"
        );
        if status(&a) == 0 {
            assert!(a.stdout == b.stdout, "{args:?}");
        }
    }
}

/// `-vv`: the filter chain, the threads and the memory needed, as in xz.
#[test]
fn debug_output_matches_xz() {
    let data = sample(100_000, 15);
    for args in [
        &["-vv", "-c", "-T1"][..],
        &["-vv", "-c", "-T3"],
        &["-vv", "-c", "-T1", "--x86", "--lzma2=preset=1"],
        &["-vv", "-c", "--format=lzma"],
        &["-vv", "-c", "-T1", "-M", "50MiB", "-9"],
        &["-vv", "-c", "-T2", "-M", "300MiB", "-8"],
        &["-vv", "-c", "-F", "raw", "--lzma1=preset=2"],
        &[
            "-vv",
            "-c",
            "-T1",
            "--delta=dist=4",
            "--arm64=start=4096",
            "--lzma2=dict=6KiB,mode=fast",
        ],
    ] {
        let a = in_c_locale_input(bin(), args, &data);
        let Ok(_) = Command::new("xz").arg("--version").output() else {
            return;
        };
        let b = in_c_locale_input(Command::new("xz"), args, &data);
        assert_eq!(
            stderr(&a).replace("xz-native:", "xz:"),
            stderr(&b).replace('\r', ""),
            "{args:?}"
        );
    }
}

/// Runs `cmd` with `input` on standard input in the C locale.
fn in_c_locale_input(mut cmd: Command, args: &[&str], input: &[u8]) -> Output {
    cmd.args(args)
        .env("LC_ALL", "C")
        .env_remove("XZ_DEFAULTS")
        .env_remove("XZ_OPT");
    run(cmd, input)
}
