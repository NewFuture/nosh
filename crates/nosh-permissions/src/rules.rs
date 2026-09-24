//! Per-command rules: what a single simple command does, given its name and
//! (already unquoted) arguments. Wrappers that run other commands (`sudo`,
//! `env`, `xargs`, `find -exec`, `bash -c`, …) are handled by the analyzer.

use crate::Risk;

/// One argument after quote removal.
#[derive(Debug, Clone)]
pub struct Arg {
    pub value: String,
    /// Contains a parameter expansion / command substitution.
    pub dynamic: bool,
    /// Contains unquoted glob characters.
    pub glob: bool,
    /// Dynamic, but only names files the analysis knows to be inside the
    /// workspace (`"$f"` in `for f in *.txt`, `{}` in `find . -exec`).
    pub bound: bool,
    /// Dynamic, but every expansion in it has a value known to the analysis
    /// (`"$KEY"` after `KEY=~/.ssh/id_rsa`): the value it will have. Only
    /// used to find protected reads; everything else still treats the
    /// argument as computed at runtime.
    pub known: Option<String>,
}

impl Arg {
    pub fn lit(s: &str) -> Self {
        Self {
            value: s.to_string(),
            dynamic: false,
            glob: false,
            bound: false,
            known: None,
        }
    }

    /// The value it will have, when the analysis knows it.
    pub fn resolved(&self) -> Option<&str> {
        if self.dynamic {
            self.known.as_deref()
        } else {
            Some(&self.value)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    pub path: String,
    pub dynamic: bool,
    pub glob: bool,
    pub bound: bool,
    /// See [`Arg::known`].
    pub known: Option<String>,
}

impl Target {
    fn of(a: &Arg) -> Self {
        Self {
            path: a.value.clone(),
            dynamic: a.dynamic,
            glob: a.glob,
            bound: a.bound,
            known: a.known.clone(),
        }
    }

    /// The part of `a` after the literal `prefix` (`if=`, `--file=`).
    fn after(a: &Arg, prefix: &str) -> Self {
        Self {
            path: a.value.get(prefix.len()..).unwrap_or_default().to_string(),
            dynamic: a.dynamic,
            glob: a.glob,
            bound: false,
            known: a
                .known
                .as_deref()
                .and_then(|k| k.strip_prefix(prefix))
                .map(str::to_string),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub risk: Risk,
    pub reason: String,
    pub writes: Vec<Target>,
    pub reads: Vec<Target>,
    pub network: bool,
    pub session: bool,
    /// Recursive delete/permission change: root/home/system targets become Forbidden/Dangerous.
    pub recursive: bool,
    pub deletes: bool,
}

impl Verdict {
    pub fn new(risk: Risk, reason: impl Into<String>) -> Self {
        Self {
            risk,
            reason: reason.into(),
            writes: vec![],
            reads: vec![],
            network: false,
            session: false,
            recursive: false,
            deletes: false,
        }
    }

    fn safe(reason: impl Into<String>) -> Self {
        Self::new(Risk::Safe, reason)
    }

    fn mutating(reason: impl Into<String>) -> Self {
        Self::new(Risk::Mutating, reason)
    }

    fn dangerous(reason: impl Into<String>) -> Self {
        Self::new(Risk::Dangerous, reason)
    }

    fn net(mut self) -> Self {
        self.network = true;
        if self.risk < Risk::Mutating {
            self.risk = Risk::Mutating;
        }
        self
    }

    fn session(mut self) -> Self {
        self.session = true;
        self
    }

    fn writes(mut self, t: impl IntoIterator<Item = Target>) -> Self {
        self.writes.extend(t);
        self
    }

    fn reads(mut self, t: impl IntoIterator<Item = Target>) -> Self {
        self.reads.extend(t);
        self
    }
}

pub fn is_opt(a: &Arg) -> bool {
    a.value.starts_with('-') && a.value.len() > 1 && !a.dynamic
}

/// Whether a short flag (possibly combined, e.g. `-rf`) or long flag is present.
pub fn has_flag(args: &[Arg], short: &[char], long: &[&str]) -> bool {
    for a in args {
        let v = a.value.as_str();
        if v == "--" {
            break;
        }
        if let Some(l) = v.strip_prefix("--") {
            let name = l.split('=').next().unwrap_or(l);
            if long.contains(&name) {
                return true;
            }
        } else if let Some(s) = v.strip_prefix('-')
            && !s.is_empty()
            && s.chars().all(|c| c.is_ascii_alphanumeric())
            && s.chars().any(|c| short.contains(&c))
        {
            return true;
        }
    }
    false
}

/// Non-option operands (everything after `--` counts as an operand).
pub fn operands(args: &[Arg]) -> Vec<&Arg> {
    let mut out = Vec::new();
    let mut end = false;
    for a in args {
        if !end && a.value == "--" {
            end = true;
            continue;
        }
        if end || !is_opt(a) {
            out.push(a);
        }
    }
    out
}

/// Operands, skipping the values of options listed in `with_value` (e.g. `-o FILE`).
pub fn operands_skipping<'a>(args: &'a [Arg], with_value: &[&str]) -> Vec<&'a Arg> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut end = false;
    while i < args.len() {
        let a = &args[i];
        if !end && a.value == "--" {
            end = true;
        } else if !end && is_opt(a) {
            if with_value.contains(&a.value.as_str()) {
                i += 1;
            }
        } else {
            out.push(a);
        }
        i += 1;
    }
    out
}

/// Value of an option (`-o X`, `-oX`, `--output X`, `--output=X`).
pub fn opt_value<'a>(args: &'a [Arg], short: Option<char>, long: &[&str]) -> Vec<&'a Arg> {
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        let v = a.value.as_str();
        if v == "--" {
            break;
        }
        if let Some(l) = v.strip_prefix("--") {
            let (name, val) = match l.split_once('=') {
                Some((n, _)) => (n, true),
                None => (l, false),
            };
            if long.contains(&name) {
                if val {
                    out.push(a);
                } else if let Some(n) = args.get(i + 1) {
                    out.push(n);
                }
            }
        } else if let Some(c) = short
            && let Some(rest) = v.strip_prefix('-')
            && rest.starts_with(c)
        {
            if rest.len() == 1 {
                if let Some(n) = args.get(i + 1) {
                    out.push(n);
                }
            } else {
                out.push(a);
            }
        }
    }
    out
}

fn strip_opt_prefix(a: &Arg, short: char, long: &[&str]) -> Target {
    let v = a.value.as_str();
    let path = if let Some(l) = v.strip_prefix("--") {
        l.split_once('=')
            .map(|(n, p)| if long.contains(&n) { p } else { v })
            .unwrap_or(v)
    } else if let Some(r) = v.strip_prefix('-').and_then(|r| r.strip_prefix(short)) {
        if r.is_empty() { v } else { r }
    } else {
        v
    };
    Target::after(a, &v[..v.len() - path.len()])
}

/// Variables whose modification changes how the session or its children behave.
pub const SENSITIVE_VARS: &[&str] = &[
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "PROMPT_COMMAND",
    "IFS",
    "BASH_ENV",
    "ENV",
    "HOME",
    "SHELL",
    "PS0",
    "PS1",
    "PS2",
    "PS3",
    "PS4",
    "HISTFILE",
    "HISTCONTROL",
    "TMOUT",
    "SHELLOPTS",
    "BASHOPTS",
    "CDPATH",
    "PYTHONPATH",
    "NODE_OPTIONS",
    "GIT_SSH_COMMAND",
    "EDITOR",
    "PAGER",
    "SUDO_ASKPASS",
    "http_proxy",
    "https_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
];

pub const KEY_VARS: &[&str] = &[
    "PATH", "HOME", "PWD", "OLDPWD", "IFS", "SHELL", "USER", "LOGNAME", "TERM", "LANG", "LC_ALL",
    "PS1", "HISTFILE", "TMPDIR",
];

pub fn var_assignment_risk(name: &str) -> Option<(Risk, String)> {
    match name {
        "LD_PRELOAD" | "LD_AUDIT" | "BASH_ENV" | "ENV" | "PROMPT_COMMAND" => Some((
            Risk::Dangerous,
            format!("sets {name} (can inject code into later commands)"),
        )),
        n if SENSITIVE_VARS.contains(&n) => {
            Some((Risk::Mutating, format!("modifies session variable {name}")))
        }
        _ => None,
    }
}

/// A builtin that assigns the variables `names`: ordinary ones are Safe,
/// like `x=1`; sensitive ones (PATH, IFS, LD_PRELOAD, …) change the session.
fn assigns_vars<'a>(what: &str, names: impl IntoIterator<Item = &'a str>) -> Verdict {
    let worst = names
        .into_iter()
        .filter_map(var_assignment_risk)
        .max_by_key(|(r, _)| *r);
    match worst {
        Some((risk, why)) => Verdict::new(risk, format!("{what}: {why}")).session(),
        None => Verdict::safe(format!("{what} (variables)")),
    }
}

/// Values of short options that take one (`-p PROMPT`, `-rp PROMPT`) and the
/// operands. `names` lists the options whose value is a variable name.
fn split_opts<'a>(args: &'a [Arg], valued: &str, names: &str) -> (Vec<&'a str>, Vec<&'a str>) {
    let (mut named, mut ops) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let v = args[i].value.as_str();
        if v == "--" {
            ops.extend(args[i + 1..].iter().map(|a| a.value.as_str()));
            break;
        }
        match v.strip_prefix('-').filter(|s| !s.is_empty()) {
            Some(cluster) => {
                for (j, c) in cluster.char_indices() {
                    if valued.contains(c) {
                        let rest = &cluster[j + c.len_utf8()..];
                        let value = if rest.is_empty() {
                            i += 1;
                            args.get(i).map(|a| a.value.as_str())
                        } else {
                            Some(rest)
                        };
                        if names.contains(c)
                            && let Some(n) = value
                        {
                            named.push(n);
                        }
                        break;
                    }
                }
            }
            None => ops.push(v),
        }
        i += 1;
    }
    (named, ops)
}

/// Builtins listed as read-only whose other forms change the session.
fn session_builtin(name: &str, args: &[Arg]) -> Option<Verdict> {
    Some(match name {
        "read" => {
            let (arrays, ops) = split_opts(args, "adinNptu", "a");
            let names = if ops.is_empty() && arrays.is_empty() {
                vec!["REPLY"]
            } else {
                arrays.into_iter().chain(ops).collect()
            };
            assigns_vars("read", names)
        }
        "mapfile" | "readarray" => {
            let (_, ops) = split_opts(args, "dnOsuCc", "");
            assigns_vars(name, [*ops.last().unwrap_or(&"MAPFILE")])
        }
        "printf" => {
            let (vars, _) = split_opts(args, "v", "v");
            if vars.is_empty() {
                Verdict::safe("prints text")
            } else {
                assigns_vars("printf -v", vars)
            }
        }
        "getopts" => {
            let ops: Vec<&str> = operands(args).iter().map(|a| a.value.as_str()).collect();
            assigns_vars("getopts", ops.get(1).copied())
        }
        "let" => {
            let names: Vec<&str> = args
                .iter()
                .filter_map(|a| {
                    let v = a.value.trim_start_matches(['+', '-']);
                    let end = v
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(v.len());
                    let rest = &v[end..];
                    let assigns = (rest.starts_with('=') && !rest.starts_with("=="))
                        || [
                            "+=", "-=", "*=", "/=", "%=", "<<=", ">>=", "&=", "^=", "|=", "++",
                            "--",
                        ]
                        .iter()
                        .any(|op| rest.starts_with(op))
                        || a.value.starts_with("++")
                        || a.value.starts_with("--");
                    (end > 0 && assigns).then_some(&v[..end])
                })
                .collect();
            assigns_vars("let", names)
        }
        "hash" => {
            if args.is_empty()
                || has_flag(args, &['l', 't'], &[]) && !has_flag(args, &['p', 'd', 'r'], &[])
            {
                Verdict::safe("lists remembered command paths")
            } else if has_flag(args, &['p'], &[]) {
                Verdict::mutating("points a command name at another program (hash -p)").session()
            } else {
                Verdict::mutating("changes the remembered command paths (hash)").session()
            }
        }
        "fc" => {
            if has_flag(args, &['l'], &[]) {
                Verdict::safe("lists history")
            } else if has_flag(args, &['s'], &[]) {
                Verdict::dangerous("re-runs a command from history (fc -s) that cannot be analyzed")
            } else {
                Verdict::mutating("edits and re-runs history commands (fc)").session()
            }
        }
        "stty" => {
            if args.is_empty()
                || args.iter().all(|a| {
                    matches!(
                        a.value.as_str(),
                        "-a" | "--all" | "-g" | "--save" | "size" | "speed"
                    )
                })
            {
                Verdict::safe("shows terminal settings")
            } else {
                Verdict::mutating("changes terminal settings (stty)").session()
            }
        }
        "mesg" => {
            if args.is_empty() {
                Verdict::safe("shows whether messages are allowed")
            } else {
                Verdict::mutating("changes whether other users may write to the terminal").session()
            }
        }
        _ => return None,
    })
}

const READERS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "nl",
    "od",
    "xxd",
    "hexdump",
    "strings",
    "file",
    "stat",
    "md5sum",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "b2sum",
    "cksum",
    "sum",
    "diff",
    "cmp",
    "comm",
    "tac",
    "rev",
    "fold",
    "fmt",
    "paste",
    "join",
    "column",
    "zcat",
    "bzcat",
    "xzcat",
    "zless",
    "zmore",
    "lz4cat",
    "zstdcat",
    "du",
    "ls",
    "dir",
    "vdir",
    "tree",
    "realpath",
    "readlink",
    "base64",
    "base32",
    "iconv",
    "expand",
    "unexpand",
    "pr",
    "look",
    "tsort",
    "ptx",
    "shuf",
    "numfmt",
    "lsattr",
    "getfacl",
    "exa",
    "eza",
    "bat",
    "batcat",
    "sort",
    "uniq",
    "cut",
    "tr",
    "split",
    "csplit",
    "tee",
    "wc",
    "namei",
    "identify",
];

const INFO: &[&str] = &[
    "pwd",
    "whoami",
    "id",
    "uname",
    "uptime",
    "w",
    "who",
    "last",
    "lastlog",
    "free",
    "nproc",
    "arch",
    "lscpu",
    "lsblk",
    "lsusb",
    "lspci",
    "lsmod",
    "lshw",
    "ps",
    "pgrep",
    "pidof",
    "top",
    "htop",
    "btop",
    "atop",
    "df",
    "printenv",
    "which",
    "whereis",
    "type",
    "locale",
    "tty",
    "groups",
    "getent",
    "ss",
    "netstat",
    "lsof",
    "jobs",
    "true",
    "false",
    "test",
    "[",
    "[[",
    "echo",
    "printf",
    "sleep",
    "seq",
    "expr",
    "cal",
    "bc",
    "dc",
    "factor",
    "yes",
    "cd",
    "pushd",
    "popd",
    "dirs",
    "help",
    "man",
    "info",
    "apropos",
    "whatis",
    "tldr",
    "nvidia-smi",
    "sensors",
    "getconf",
    "lsb_release",
    "vmstat",
    "iostat",
    "mpstat",
    "sar",
    "pmap",
    "basename",
    "dirname",
    ":",
    "wait",
    "read",
    "hash",
    "compgen",
    "caller",
    "times",
    "let",
    "printf",
    "logname",
    "users",
    "mesg",
    "tput",
    "stty",
    "clear",
    "reset",
    "neofetch",
    "fastfetch",
    "screenfetch",
    "ulimit",
    "umask",
    "getopts",
    "shift",
    "return",
    "break",
    "continue",
    "local",
    "fc",
    "kill_list",
    "lslocks",
    "lsipc",
    "findmnt",
    "blkid",
    "dmidecode",
    "fwupdmgr",
    "numactl",
];

const NETWORK: &[&str] = &[
    "curl",
    "wget",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "telnet",
    "ftp",
    "lftp",
    "ping",
    "ping6",
    "traceroute",
    "tracepath",
    "mtr",
    "dig",
    "nslookup",
    "host",
    "whois",
    "nmap",
    "http",
    "https",
    "xh",
    "aria2c",
    "axel",
    "ssh-copy-id",
    "sshpass",
    "mosh",
    "rclone",
    "s3cmd",
    "gsutil",
    "az",
    "aws",
    "gcloud",
    "doctl",
    "tailscale",
    "wg",
];

fn first_word(args: &[Arg]) -> Option<&str> {
    operands(args).first().map(|a| a.value.as_str())
}

pub fn classify(name: &str, args: &[Arg]) -> Verdict {
    if let Some(v) = session_builtin(name, args) {
        return v;
    }
    let ops = || operands(args);
    let targets = |v: Vec<&Arg>| v.into_iter().map(Target::of).collect::<Vec<_>>();
    match name {
        "rm" => {
            if has_flag(args, &[], &["no-preserve-root"]) {
                return Verdict::new(Risk::Forbidden, "rm --no-preserve-root");
            }
            let recursive = has_flag(args, &['r', 'R'], &["recursive"]);
            let force = has_flag(args, &['f'], &["force"]);
            let mut v = if recursive || force {
                Verdict::dangerous(if recursive {
                    "recursively deletes files (rm -r)"
                } else {
                    "force-deletes files (rm -f)"
                })
            } else {
                Verdict::mutating("deletes files")
            };
            v.recursive = recursive;
            v.deletes = true;
            v.writes(targets(ops()))
        }
        "rmdir" | "unlink" => {
            let mut v = Verdict::mutating("deletes files").writes(targets(ops()));
            v.deletes = true;
            v
        }
        "shred" | "wipe" | "srm" => {
            Verdict::dangerous("irrecoverably overwrites files").writes(targets(ops()))
        }
        "truncate" => Verdict::dangerous("truncates files (data loss)").writes(targets(
            operands_skipping(args, &["-s", "-r", "--size", "--reference"]),
        )),
        "mv" => {
            let ops = ops();
            let to_null = ops.last().is_some_and(|a| a.value == "/dev/null");
            let mut v = if to_null {
                Verdict::dangerous("moves files to /dev/null (destroys them)")
            } else {
                Verdict::mutating("moves/renames files")
            };
            v.deletes = to_null;
            let mut t = targets(ops);
            t.extend(targets(opt_value(args, Some('t'), &["target-directory"])));
            v.writes(t)
        }
        "cp" | "install" | "ln" | "link" => {
            let ops = ops();
            let mut writes = targets(opt_value(args, Some('t'), &["target-directory"]));
            let mut reads = Vec::new();
            if writes.is_empty() {
                if let Some((last, rest)) = ops.split_last() {
                    writes.push(Target::of(last));
                    reads = targets(rest.to_vec());
                }
            } else {
                reads = targets(ops);
            }
            Verdict::mutating(format!("{name}: writes files"))
                .writes(writes)
                .reads(reads)
        }
        "mkdir" | "touch" | "mkfifo" | "mknod" | "mktemp" => {
            Verdict::mutating("creates files or directories").writes(targets(operands_skipping(
                args,
                &["-m", "--mode", "-t", "-d", "-r", "--reference", "-p"],
            )))
        }
        "tee" => Verdict::mutating("writes files (tee)").writes(targets(ops())),
        "dd" => {
            let mut v = Verdict::dangerous("raw data copy (dd)");
            for a in args {
                if let Some(of) = a.value.strip_prefix("of=") {
                    v.writes.push(Target {
                        glob: false,
                        ..Target::after(a, "of=")
                    });
                    if is_disk_device(of) {
                        return Verdict::new(
                            Risk::Forbidden,
                            format!("dd overwrites disk device {of}"),
                        );
                    }
                } else if a.value.starts_with("if=") {
                    v.reads.push(Target {
                        glob: false,
                        ..Target::after(a, "if=")
                    });
                }
            }
            v
        }
        n if n.starts_with("mkfs") || n == "mke2fs" || n == "mkswap" || n == "mkntfs" => {
            if ops().iter().any(|a| is_disk_device(&a.value)) {
                Verdict::new(Risk::Forbidden, format!("{n} formats a disk device"))
            } else {
                Verdict::dangerous(format!("{n} creates a filesystem (destroys data)"))
            }
        }
        "fdisk" | "sfdisk" | "gdisk" | "sgdisk" | "cfdisk" | "parted" | "wipefs" | "blkdiscard" => {
            Verdict::dangerous("modifies disk partitions")
        }
        "chmod" | "chown" | "chgrp" | "chattr" | "setfacl" => {
            let recursive = has_flag(args, &['R'], &["recursive"]);
            let ops = operands_skipping(args, &["--reference"]);
            let files: Vec<&Arg> = if name == "chattr" || name == "setfacl" {
                ops
            } else {
                ops.into_iter().skip(1).collect()
            };
            let mut v = Verdict::mutating(format!("changes file ownership/permissions ({name})"))
                .writes(targets(files));
            v.recursive = recursive;
            v
        }
        "sed" => {
            let inplace = has_flag(args, &['i'], &["in-place"])
                || args.iter().any(|a| a.value.starts_with("-i"));
            let ops = operands_skipping(
                args,
                &["-e", "-f", "--expression", "--file", "-l", "--line-length"],
            );
            let script = if opt_value(args, Some('e'), &["expression"]).is_empty() {
                ops.first().map(|a| a.value.clone())
            } else {
                None
            };
            let files: Vec<&Arg> = if script.is_some() {
                ops.into_iter().skip(1).collect()
            } else {
                ops
            };
            let s = script.unwrap_or_default();
            if inplace {
                Verdict::mutating("edits files in place (sed -i)").writes(targets(files))
            } else if sed_script_writes(&s) {
                Verdict::mutating("sed script writes files or runs commands")
            } else {
                Verdict::safe("sed (read-only)").reads(targets(files))
            }
        }
        "awk" | "gawk" | "mawk" | "nawk" => {
            let ops = operands_skipping(
                args,
                &["-f", "-v", "-F", "--file", "--assign", "--field-separator"],
            );
            let prog = ops.first().map(|a| a.value.as_str()).unwrap_or("");
            if prog.contains("system(")
                || prog.contains("| getline")
                || prog.contains("|getline")
                || awk_redirects(prog)
                || has_flag(args, &['i'], &["in-place"])
            {
                Verdict::mutating("awk program runs commands or writes files")
            } else {
                Verdict::safe("awk (read-only)").reads(targets(ops.into_iter().skip(1).collect()))
            }
        }
        "sort" => {
            let out = opt_value(args, Some('o'), &["output"]);
            if out.is_empty() {
                Verdict::safe("sort").reads(targets(ops()))
            } else {
                Verdict::mutating("sort writes an output file").writes(
                    out.into_iter()
                        .map(|a| strip_opt_prefix(a, 'o', &["output"]))
                        .collect::<Vec<_>>(),
                )
            }
        }
        "uniq" => {
            let ops = ops();
            if ops.len() >= 2 {
                Verdict::mutating("uniq writes an output file").writes(vec![Target::of(ops[1])])
            } else {
                Verdict::safe("uniq").reads(targets(ops))
            }
        }
        "split" | "csplit" => Verdict::mutating("splits into new files"),
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "zgrep" | "jq" | "yq" | "xmllint" => {
            let ops = operands_skipping(
                args,
                &[
                    "-e",
                    "-f",
                    "-m",
                    "-A",
                    "-B",
                    "-C",
                    "--regexp",
                    "--file",
                    "-g",
                    "--glob",
                    "-t",
                    "--type",
                    "--max-count",
                    "--arg",
                    "--argjson",
                ],
            );
            let files: Vec<&Arg> = if opt_value(args, Some('e'), &["regexp"]).is_empty() {
                ops.into_iter().skip(1).collect()
            } else {
                ops
            };
            Verdict::safe("search (read-only)").reads(targets(files))
        }
        "find" => {
            // -exec/-delete are handled by the analyzer; here only output files.
            let mut v = Verdict::safe("find (read-only)");
            for (i, a) in args.iter().enumerate() {
                if matches!(
                    a.value.as_str(),
                    "-fprint" | "-fprint0" | "-fls" | "-fprintf"
                ) && let Some(f) = args.get(i + 1)
                {
                    v = Verdict::mutating("find writes an output file").writes(vec![Target::of(f)]);
                }
            }
            v
        }
        "tar" | "bsdtar" => {
            let joined: String = args.iter().take(1).map(|a| a.value.clone()).collect();
            let list = has_flag(args, &['t'], &["list"])
                || (!joined.starts_with('-')
                    && joined.contains('t')
                    && !joined.contains('x')
                    && !joined.contains('c'));
            if list {
                Verdict::safe("lists an archive")
            } else {
                let mut w = targets(opt_value(args, Some('C'), &["directory"]));
                w.extend(targets(opt_value(args, Some('f'), &["file"])));
                Verdict::mutating("creates or extracts an archive").writes(w)
            }
        }
        "unzip" => {
            if has_flag(args, &['l', 'v', 'Z'], &[]) {
                Verdict::safe("lists an archive")
            } else {
                Verdict::mutating("extracts an archive").writes(targets(opt_value(
                    args,
                    Some('d'),
                    &[],
                )))
            }
        }
        "zip" | "gzip" | "gunzip" | "bzip2" | "bunzip2" | "xz" | "unxz" | "zstd" | "unzstd"
        | "lz4" | "7z" | "7za" | "rar" | "unrar" | "compress" | "uncompress" => {
            if has_flag(args, &['l', 't'], &["list", "test"]) && !name.starts_with("7z") {
                Verdict::safe("inspects an archive")
            } else {
                Verdict::mutating("compresses or extracts files").writes(targets(ops()))
            }
        }
        "git" => git(args),
        "docker" | "podman" | "nerdctl" => docker(args),
        "kubectl" | "oc" | "helm" => kubectl(name, args),
        "systemctl" => systemctl(args),
        "service" => {
            if args
                .iter()
                .any(|a| a.value == "status" || a.value == "--status-all")
            {
                Verdict::safe("service status")
            } else {
                Verdict::mutating("controls a system service")
            }
        }
        "journalctl" => {
            if args.iter().any(|a| {
                a.value.starts_with("--vacuum") || a.value == "--rotate" || a.value == "--flush"
            }) {
                Verdict::mutating("modifies the journal")
            } else {
                Verdict::safe("reads logs")
            }
        }
        "dmesg" => {
            if has_flag(args, &['c', 'C'], &["clear", "read-clear"]) {
                Verdict::mutating("clears the kernel log")
            } else {
                Verdict::safe("reads the kernel log")
            }
        }
        "shutdown" | "reboot" | "poweroff" | "halt" | "init" | "telinit" | "kexec" => {
            Verdict::dangerous("shuts down or reboots the machine")
        }
        "kill" | "pkill" | "killall" | "skill" | "xkill" => {
            if args.iter().any(|a| a.value == "$$" || a.value == "$PPID") {
                return Verdict::new(Risk::Forbidden, "kills the nosh shell itself");
            }
            if name == "kill" && ops().iter().any(|a| a.value == "-1" || a.value == "1") {
                Verdict::dangerous("signals init or every process")
            } else if name == "kill" && args.iter().any(|a| a.value == "-l" || a.value == "-L") {
                Verdict::safe("lists signals")
            } else {
                Verdict::mutating("sends a signal to processes")
            }
        }
        "killall5" => Verdict::dangerous("signals every process"),
        "crontab" => {
            if has_flag(args, &['l'], &[]) {
                Verdict::safe("lists cron jobs")
            } else if has_flag(args, &['r'], &[]) {
                Verdict::dangerous("removes all cron jobs")
            } else {
                Verdict::mutating("changes scheduled jobs")
            }
        }
        "at" | "batch" | "atrm" => Verdict::mutating("schedules jobs"),
        "useradd" | "userdel" | "usermod" | "groupadd" | "groupdel" | "groupmod" | "passwd"
        | "chpasswd" | "visudo" | "vipw" | "chsh" | "chfn" | "adduser" | "deluser" | "gpasswd"
        | "newgrp" => Verdict::dangerous("changes users, groups or credentials"),
        "mount" | "umount" | "swapon" | "swapoff" | "losetup" | "cryptsetup" | "lvremove"
        | "vgremove" | "pvremove" | "mdadm" | "zpool" | "zfs" | "btrfs" => {
            if args.is_empty() && name == "mount" {
                Verdict::safe("lists mounts")
            } else {
                Verdict::dangerous("changes storage configuration")
            }
        }
        "iptables" | "ip6tables" | "nft" | "ufw" | "firewall-cmd" | "iptables-restore" => {
            if has_flag(args, &['L', 'S'], &["list", "list-rules"])
                || first_word(args) == Some("status")
                || first_word(args) == Some("list")
            {
                Verdict::safe("lists firewall rules")
            } else {
                Verdict::dangerous("changes firewall rules")
            }
        }
        "modprobe" | "insmod" | "rmmod" | "depmod" => {
            Verdict::dangerous("loads or unloads kernel modules")
        }
        "sysctl" => {
            if has_flag(args, &['w'], &["write", "load", "system"])
                || args.iter().any(|a| a.value.contains('='))
            {
                Verdict::dangerous("changes kernel parameters")
            } else {
                Verdict::safe("reads kernel parameters")
            }
        }
        "hostname" | "hostnamectl" => {
            if ops().is_empty() || first_word(args) == Some("status") {
                Verdict::safe("shows the hostname")
            } else {
                Verdict::dangerous("changes the hostname")
            }
        }
        "date" => {
            if has_flag(args, &['s'], &["set"]) {
                Verdict::dangerous("sets the system clock")
            } else {
                Verdict::safe("date")
            }
        }
        "timedatectl" | "hwclock" | "localectl" => {
            if ops().is_empty()
                || first_word(args) == Some("status")
                || has_flag(args, &['r'], &["show"])
            {
                Verdict::safe("shows system settings")
            } else {
                Verdict::dangerous("changes system time/locale settings")
            }
        }
        "ip" => {
            let w: Vec<&str> = ops().iter().map(|a| a.value.as_str()).collect();
            let mutating = w.iter().any(|x| {
                matches!(
                    *x,
                    "add" | "del" | "delete" | "set" | "change" | "replace" | "flush" | "append"
                )
            });
            if mutating {
                Verdict::dangerous("changes network configuration")
            } else {
                Verdict::safe("shows network configuration")
            }
        }
        "ifconfig" | "route" | "iwconfig" | "nmcli" | "arp" => {
            if ops().len() <= 1
                && !args.iter().any(|a| {
                    matches!(
                        a.value.as_str(),
                        "up" | "down"
                            | "add"
                            | "del"
                            | "delete"
                            | "set"
                            | "modify"
                            | "connect"
                            | "disconnect"
                    )
                })
            {
                Verdict::safe("shows network configuration")
            } else {
                Verdict::dangerous("changes network configuration")
            }
        }
        "apt" | "apt-get" | "aptitude" | "dpkg" | "dnf" | "yum" | "zypper" | "pacman" | "apk"
        | "snap" | "flatpak" | "brew" | "port" | "rpm" | "emerge" | "nix-env" | "pkg" => {
            package_manager(name, args)
        }
        "npm" | "pnpm" | "yarn" | "bun" => js_pm(args),
        "npx" | "pnpx" | "bunx" => Verdict::mutating("downloads and runs a package").net(),
        "pip" | "pip3" | "pipx" | "uv" | "poetry" | "conda" | "mamba" | "gem" | "bundle"
        | "composer" => py_pm(args),
        "cargo" => match first_word(args) {
            Some(
                "--version" | "-V" | "version" | "tree" | "metadata" | "locate-project"
                | "verify-project" | "pkgid" | "help",
            )
            | None => Verdict::safe("cargo (read-only)"),
            Some("publish" | "login" | "install" | "update" | "add" | "search" | "fetch") => {
                Verdict::mutating("cargo (network)").net()
            }
            _ => Verdict::mutating("cargo builds or runs code"),
        },
        "go" => match first_word(args) {
            Some("version" | "env" | "list" | "doc" | "help" | "vet") | None => {
                Verdict::safe("go (read-only)")
            }
            Some("get" | "install" | "mod") => Verdict::mutating("go (network)").net(),
            _ => Verdict::mutating("go builds or runs code"),
        },
        "make" | "cmake" | "ninja" | "meson" | "gradle" | "gradlew" | "mvn" | "ant" | "bazel"
        | "sbt" | "just" | "task" | "rake" | "ctest" | "scons" => {
            if has_flag(
                args,
                &['n', 'v'],
                &["dry-run", "version", "help", "just-print"],
            ) {
                Verdict::safe("build tool (dry run / info)")
            } else {
                Verdict::mutating("runs a build")
            }
        }
        "python" | "python2" | "python3" | "node" | "deno" | "ruby" | "perl" | "php" | "lua"
        | "luajit" | "Rscript" | "java" | "dotnet" | "julia" | "ghc" | "runghc" | "tclsh"
        | "swift" | "kotlin" | "scala" | "elixir" | "erl" | "irb" | "ts-node" | "tsx" => {
            if args.len() == 1
                && matches!(
                    args[0].value.as_str(),
                    "--version" | "-V" | "-v" | "version" | "--help" | "-h"
                )
            {
                Verdict::safe("prints version")
            } else {
                Verdict::mutating(format!("runs a {name} program"))
            }
        }
        n if n.starts_with("python3.") || n.starts_with("python2.") => {
            if args.len() == 1 && matches!(args[0].value.as_str(), "--version" | "-V") {
                Verdict::safe("prints version")
            } else {
                Verdict::mutating("runs a python program")
            }
        }
        "vi" | "vim" | "nvim" | "nano" | "emacs" | "ed" | "ex" | "code" | "gedit" | "micro"
        | "helix" | "hx" | "kate" | "joe" | "mcedit" | "pico" => {
            Verdict::mutating("opens an interactive editor")
        }
        "ssh-keygen" | "ssh-add" | "gpg" | "gpg2" | "openssl" | "certbot" | "keytool" => {
            if args.iter().any(|a| {
                matches!(
                    a.value.as_str(),
                    "version"
                        | "--version"
                        | "-l"
                        | "--list-keys"
                        | "-L"
                        | "--fingerprint"
                        | "--list-secret-keys"
                        | "-K"
                )
            }) {
                Verdict::safe("lists keys / version")
            } else {
                Verdict::mutating("manages keys or certificates")
            }
        }
        "chroot" | "nsenter" | "unshare" | "setcap" | "capsh" => {
            Verdict::dangerous("changes process isolation or capabilities")
        }
        "history" => {
            if has_flag(args, &['c', 'd', 'w', 'r', 'a', 'n', 's'], &[]) {
                Verdict::mutating("modifies shell history").session()
            } else {
                Verdict::safe("shows history")
            }
        }
        "alias" => {
            if args.iter().any(|a| a.value.contains('=')) {
                Verdict::mutating("defines an alias").session()
            } else {
                Verdict::safe("lists aliases")
            }
        }
        "unalias" => Verdict::mutating("removes aliases").session(),
        "set" => {
            if args.is_empty() || (args.len() == 1 && matches!(args[0].value.as_str(), "-o" | "+o"))
            {
                Verdict::safe("shows shell options")
            } else {
                Verdict::mutating("changes shell options (set)").session()
            }
        }
        "shopt" => {
            if has_flag(args, &['s', 'u'], &[]) {
                Verdict::mutating("changes shell options (shopt)").session()
            } else {
                Verdict::safe("shows shell options")
            }
        }
        "trap" => {
            if args.is_empty() || has_flag(args, &['p', 'l'], &[]) {
                Verdict::safe("lists traps")
            } else {
                Verdict::mutating("sets a signal trap").session()
            }
        }
        "ulimit_set" => Verdict::mutating("changes resource limits").session(),
        "umask_set" => Verdict::mutating("changes the file creation mask").session(),
        "bind" | "enable" | "complete" | "compopt" => {
            if args.is_empty()
                || has_flag(args, &['p', 'P', 'l', 'v', 'V', 's', 'S', 'a'], &[])
                    && name != "enable"
            {
                Verdict::safe("shows shell settings")
            } else {
                Verdict::mutating("changes shell settings").session()
            }
        }
        "fg" | "bg" | "disown" | "suspend" => {
            Verdict::mutating("changes job control state").session()
        }
        "source" | "." => Verdict::mutating("sources a script into the session")
            .session()
            .reads(targets(ops().into_iter().take(1).collect())),
        "env" | "printenv" => Verdict::safe("prints the environment"),
        n if READERS.contains(&n) => {
            if n == "tee" {
                Verdict::mutating("writes files (tee)").writes(targets(ops()))
            } else if n == "split" || n == "csplit" {
                Verdict::mutating("splits into new files")
            } else {
                Verdict::safe("read-only").reads(targets(ops()))
            }
        }
        n if INFO.contains(&n) => Verdict::safe("read-only"),
        n if NETWORK.contains(&n) => network_tool(n, args),
        _ => Verdict::mutating(format!("unknown command '{name}'; assumed to modify state")),
    }
}

pub fn is_disk_device(p: &str) -> bool {
    let Some(p) = p.strip_prefix("/dev/") else {
        return false;
    };
    [
        "sd", "hd", "vd", "xvd", "nvme", "mmcblk", "disk", "md", "dm-", "mapper/", "loop",
    ]
    .iter()
    .any(|pre| p.starts_with(pre))
}

fn sed_script_writes(s: &str) -> bool {
    // `w file`, `W file`, `e cmd` commands, or `s///w file` / `s///e` flags.
    let t = s.trim();
    t.starts_with("w ")
        || t.starts_with("W ")
        || t.starts_with("e ")
        || t == "e"
        || t.contains(";w ")
        || t.contains(";e ")
        || (t.starts_with('s') && {
            let flags = t
                .rsplit(t.chars().nth(1).unwrap_or('/'))
                .next()
                .unwrap_or("");
            flags.contains('w') || flags.contains('e')
        })
}

fn awk_redirects(prog: &str) -> bool {
    // print/printf with > or >> to a file, or | to a command.
    let mut in_str = false;
    let b = prog.as_bytes();
    for (i, c) in b.iter().enumerate() {
        match c {
            b'"' => in_str = !in_str,
            b'>' | b'|' if !in_str => {
                let before = &prog[..i];
                if before.contains("print") {
                    let after = prog[i + 1..].trim_start();
                    // Comparisons like `$1 > 5` are not redirects; a quoted target or variable after print is.
                    if after.starts_with('"') || after.starts_with('>') || *c == b'|' {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn git(args: &[Arg]) -> Verdict {
    let sub_idx = args.iter().position(|a| !is_opt(a) || a.value == "--");
    // Skip global options with values: -C dir, -c k=v, --git-dir x, --work-tree x.
    let mut i = 0;
    while i < args.len() {
        match args[i].value.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => i += 2,
            v if v.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let _ = sub_idx;
    let Some(sub) = args.get(i).map(|a| a.value.as_str()) else {
        return Verdict::safe("git");
    };
    let rest = &args[i + 1..];
    let fl = |s: &[char], l: &[&str]| has_flag(rest, s, l);
    match sub {
        "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" | "ls-tree" | "blame"
        | "describe" | "grep" | "shortlog" | "cat-file" | "show-ref" | "rev-list"
        | "whatchanged" | "count-objects" | "help" | "version" | "--version" | "var"
        | "check-ignore" | "name-rev" | "merge-base" | "cherry" | "for-each-ref" | "annotate"
        | "difftool" | "range-diff" | "verify-commit" | "verify-tag" | "check-attr" | "fsck" => {
            Verdict::safe("git (read-only)")
        }
        "branch" => {
            if fl(&['D'], &[]) || (fl(&['d'], &["delete"]) && fl(&['f'], &["force"])) {
                Verdict::dangerous("force-deletes a git branch")
            } else if fl(
                &['d', 'm', 'M', 'c', 'C', 'u'],
                &[
                    "delete",
                    "move",
                    "copy",
                    "set-upstream-to",
                    "unset-upstream",
                    "edit-description",
                ],
            ) || !operands(rest).is_empty()
            {
                Verdict::mutating("changes git branches")
            } else {
                Verdict::safe("lists git branches")
            }
        }
        "tag" => {
            if operands(rest).is_empty() || fl(&['l'], &["list"]) {
                Verdict::safe("lists git tags")
            } else if fl(&['d'], &["delete"]) {
                Verdict::dangerous("deletes git tags")
            } else {
                Verdict::mutating("creates git tags")
            }
        }
        "remote" => match operands(rest).first().map(|a| a.value.as_str()) {
            None | Some("show" | "get-url") => Verdict::safe("shows git remotes"),
            Some("update" | "prune") => Verdict::mutating("git remote (network)").net(),
            _ => Verdict::mutating("changes git remotes"),
        },
        "config" => {
            if fl(
                &['l'],
                &["list", "get", "get-all", "get-regexp", "show-origin"],
            ) || operands(rest).len() <= 1
            {
                Verdict::safe("reads git config")
            } else {
                Verdict::mutating("changes git config")
            }
        }
        "stash" => match operands(rest).first().map(|a| a.value.as_str()) {
            Some("list" | "show") => Verdict::safe("shows stashes"),
            Some("drop" | "clear") => Verdict::dangerous("discards stashed changes"),
            _ => Verdict::mutating("stashes changes"),
        },
        "reflog" => match operands(rest).first().map(|a| a.value.as_str()) {
            Some("expire" | "delete") => Verdict::dangerous("deletes reflog entries"),
            _ => Verdict::safe("shows the reflog"),
        },
        "worktree" => match operands(rest).first().map(|a| a.value.as_str()) {
            Some("list") => Verdict::safe("lists worktrees"),
            Some("remove" | "prune") => Verdict::dangerous("removes worktrees"),
            _ => Verdict::mutating("changes worktrees"),
        },
        "submodule" => match operands(rest).first().map(|a| a.value.as_str()) {
            None | Some("status" | "summary") => Verdict::safe("shows submodules"),
            _ => Verdict::mutating("updates submodules (network)").net(),
        },
        "push" => {
            let force = fl(
                &['f'],
                &[
                    "force",
                    "force-with-lease",
                    "force-if-includes",
                    "mirror",
                    "delete",
                    "prune",
                ],
            ) || operands(rest)
                .iter()
                .any(|a| a.value.starts_with('+') || a.value.starts_with(':'));
            if force {
                Verdict::dangerous("force-pushes or deletes remote refs").net()
            } else {
                Verdict::mutating("pushes to a remote").net()
            }
        }
        "fetch" | "pull" | "clone" | "ls-remote" | "archive" => {
            Verdict::mutating(format!("git {sub} (network)")).net()
        }
        "reset" => {
            if fl(&[], &["hard", "merge", "keep"]) {
                Verdict::dangerous("discards changes (git reset --hard)")
            } else {
                Verdict::mutating("moves HEAD (git reset)")
            }
        }
        "clean" => {
            if fl(&['n'], &["dry-run"]) {
                Verdict::safe("previews git clean")
            } else {
                Verdict::dangerous("deletes untracked files (git clean)")
            }
        }
        "checkout" | "restore" => {
            let discards = fl(&['f'], &["force", "overlay", "worktree"]) && sub == "checkout"
                || operands(rest)
                    .iter()
                    .any(|a| a.value == "." || a.value == "--")
                || rest.iter().any(|a| a.value == "--")
                || (sub == "restore" && !fl(&['S'], &["staged"]));
            if discards {
                Verdict::dangerous("discards local changes")
            } else {
                Verdict::mutating("switches branches or restores files")
            }
        }
        "gc" | "prune" | "filter-branch" | "filter-repo" | "replace" | "update-ref" => {
            if fl(&[], &["prune"]) || sub != "gc" {
                Verdict::dangerous(format!("rewrites or prunes git history (git {sub})"))
            } else {
                Verdict::mutating("git gc")
            }
        }
        "rm" => {
            if fl(&['r', 'f'], &["force"]) {
                Verdict::dangerous("deletes tracked files (git rm -r/-f)")
            } else {
                Verdict::mutating("removes tracked files")
            }
        }
        "lfs" => match operands(rest).first().map(|a| a.value.as_str()) {
            Some("ls-files" | "status" | "env" | "version") => Verdict::safe("git lfs (read-only)"),
            _ => Verdict::mutating("git lfs (network)").net(),
        },
        "bisect" => match operands(rest).first().map(|a| a.value.as_str()) {
            Some("log" | "view" | "visualize") => Verdict::safe("git bisect log"),
            _ => Verdict::mutating("git bisect"),
        },
        _ => Verdict::mutating(format!("git {sub} changes the repository")),
    }
}

fn docker(args: &[Arg]) -> Verdict {
    let ops = operands_skipping(
        args,
        &[
            "-H",
            "--host",
            "--context",
            "--config",
            "-c",
            "-l",
            "--log-level",
        ],
    );
    let words: Vec<&str> = ops.iter().map(|a| a.value.as_str()).collect();
    let (a, b) = (words.first().copied(), words.get(1).copied());
    let force = has_flag(args, &['f'], &["force", "all", "volumes"]);
    match (a, b) {
        (None, _)
        | (
            Some(
                "ps" | "images" | "logs" | "inspect" | "version" | "info" | "top" | "port" | "diff"
                | "history" | "events" | "search",
            ),
            _,
        ) => Verdict::safe("docker (read-only)"),
        (Some("stats"), _) => Verdict::safe("docker stats"),
        (
            Some(
                "container" | "image" | "network" | "volume" | "context" | "plugin" | "node"
                | "service" | "secret" | "config",
            ),
            Some("ls" | "list" | "inspect" | "ps" | "logs" | "history"),
        ) => Verdict::safe("docker (read-only)"),
        (
            Some("compose" | "stack"),
            Some("ps" | "logs" | "config" | "ls" | "images" | "top" | "version"),
        ) => Verdict::safe("docker compose (read-only)"),
        (
            Some("system" | "container" | "image" | "volume" | "network" | "builder" | "buildx"),
            Some("prune"),
        ) => Verdict::dangerous("prunes docker resources"),
        (Some("system"), Some("df" | "info" | "events")) => Verdict::safe("docker system info"),
        (Some("volume"), Some("rm")) => Verdict::dangerous("deletes docker volumes (data loss)"),
        (Some("rm" | "rmi"), _) if force => {
            Verdict::dangerous("force-removes containers or images")
        }
        (Some("compose"), Some("down")) if has_flag(args, &['v'], &["volumes"]) => {
            Verdict::dangerous("removes containers and volumes")
        }
        (Some("pull" | "push" | "login" | "logout" | "build" | "run"), _) => {
            Verdict::mutating(format!("docker {}", a.unwrap_or(""))).net()
        }
        _ => Verdict::mutating(format!("docker {} changes containers", a.unwrap_or(""))),
    }
}

fn kubectl(name: &str, args: &[Arg]) -> Verdict {
    let ops = operands_skipping(
        args,
        &[
            "-n",
            "--namespace",
            "--context",
            "--kubeconfig",
            "-l",
            "--selector",
            "-o",
            "--output",
            "-c",
            "--container",
        ],
    );
    let a = ops.first().map(|a| a.value.as_str());
    let b = ops.get(1).map(|a| a.value.as_str());
    if name == "helm" {
        return match a {
            None
            | Some(
                "list" | "ls" | "status" | "get" | "history" | "show" | "search" | "version"
                | "template" | "lint" | "env",
            ) => Verdict::safe("helm (read-only)"),
            Some("uninstall" | "delete" | "rollback") => {
                Verdict::dangerous("removes or rolls back a release").net()
            }
            _ => Verdict::mutating("helm changes releases").net(),
        };
    }
    match (a, b) {
        (
            None
            | Some(
                "get" | "describe" | "logs" | "top" | "version" | "explain" | "api-resources"
                | "api-versions" | "cluster-info" | "events" | "diff",
            ),
            _,
        ) => Verdict::safe("kubectl (read-only)").net(),
        (
            Some("config"),
            Some("view" | "get-contexts" | "current-context" | "get-clusters" | "get-users"),
        ) => Verdict::safe("kubectl config (read-only)"),
        (Some("auth"), Some("can-i" | "whoami")) => Verdict::safe("kubectl auth check").net(),
        (Some("delete" | "drain" | "replace"), _) => {
            Verdict::dangerous(format!("kubectl {} (destructive)", a.unwrap_or(""))).net()
        }
        _ => Verdict::mutating(format!("kubectl {} changes the cluster", a.unwrap_or(""))).net(),
    }
}

fn systemctl(args: &[Arg]) -> Verdict {
    match first_word(args) {
        None
        | Some(
            "status" | "list-units" | "list-unit-files" | "list-timers" | "list-sockets"
            | "list-dependencies" | "list-jobs" | "show" | "cat" | "is-active" | "is-enabled"
            | "is-failed" | "is-system-running" | "help" | "get-default",
        ) => Verdict::safe("systemctl (read-only)"),
        Some(
            "poweroff" | "reboot" | "halt" | "kexec" | "rescue" | "emergency" | "suspend"
            | "hibernate" | "hybrid-sleep" | "isolate" | "default",
        ) => Verdict::dangerous("changes the system run state"),
        _ => Verdict::mutating("controls system services"),
    }
}

fn package_manager(name: &str, args: &[Arg]) -> Verdict {
    let sub = first_word(args).unwrap_or("");
    let read_only = match name {
        "dpkg" => has_flag(
            args,
            &['l', 'L', 's', 'S', 'p'],
            &[
                "list",
                "listfiles",
                "status",
                "search",
                "print-avail",
                "get-selections",
            ],
        ),
        "rpm" => has_flag(args, &['q'], &["query"]),
        "pacman" => {
            has_flag(args, &['Q', 'F'], &["query", "files"])
                || (has_flag(args, &['S'], &[])
                    && has_flag(args, &['s', 'i'], &[])
                    && !has_flag(args, &['y', 'u'], &[]))
        }
        _ => matches!(
            sub,
            "list"
                | "search"
                | "show"
                | "info"
                | "policy"
                | "depends"
                | "rdepends"
                | "madison"
                | "changelog"
                | "--version"
                | "-v"
                | "--help"
                | "help"
                | "list-installed"
                | "outdated"
                | "deps"
                | "leaves"
                | "doctor"
                | "config"
                | "home"
                | "desc"
                | "uses"
                | "query"
                | "why"
                | "provides"
                | "repolist"
        ),
    };
    if read_only || args.is_empty() {
        Verdict::safe(format!("{name} (read-only)"))
    } else {
        Verdict::mutating(format!("{name} installs or removes packages")).net()
    }
}

fn js_pm(args: &[Arg]) -> Verdict {
    match first_word(args) {
        None
        | Some(
            "ls" | "list" | "ll" | "la" | "view" | "info" | "show" | "outdated" | "--version"
            | "-v" | "version" | "help" | "root" | "bin" | "prefix" | "why" | "explain" | "config"
            | "get" | "search" | "docs" | "repo" | "whoami",
        ) => Verdict::safe("package info"),
        Some(
            "install" | "i" | "ci" | "add" | "update" | "upgrade" | "up" | "publish" | "login"
            | "logout" | "audit" | "dlx" | "create" | "init" | "exec" | "x",
        ) => Verdict::mutating("package manager (network)").net(),
        Some(
            "uninstall" | "remove" | "rm" | "un" | "r" | "prune" | "dedupe" | "link" | "unlink"
            | "cache" | "pack",
        ) => Verdict::mutating("package manager changes files"),
        _ => Verdict::mutating("runs a package script"),
    }
}

fn py_pm(args: &[Arg]) -> Verdict {
    match first_word(args) {
        None
        | Some(
            "list" | "show" | "freeze" | "--version" | "-V" | "version" | "check" | "help" | "env"
            | "info" | "search" | "outdated" | "tree" | "which" | "inspect" | "debug",
        ) => Verdict::safe("package info"),
        Some(
            "install" | "download" | "wheel" | "add" | "sync" | "lock" | "update" | "upgrade"
            | "publish" | "create" | "pip",
        ) => Verdict::mutating("package manager (network)").net(),
        Some("uninstall" | "remove" | "cache" | "purge") => {
            Verdict::mutating("package manager removes packages")
        }
        _ => Verdict::mutating("package manager changes the environment"),
    }
}

fn network_tool(name: &str, args: &[Arg]) -> Verdict {
    let mut v = Verdict::mutating(format!("network access ({name})")).net();
    match name {
        "curl" => {
            v.writes.extend(
                opt_value(args, Some('o'), &["output"])
                    .into_iter()
                    .map(|a| strip_opt_prefix(a, 'o', &["output"])),
            );
            v.writes.extend(
                opt_value(args, Some('c'), &["cookie-jar"])
                    .into_iter()
                    .map(|a| strip_opt_prefix(a, 'c', &["cookie-jar"])),
            );
            for a in opt_value(args, Some('T'), &["upload-file"]) {
                v.reads.push(Target::of(a));
            }
            for a in args {
                if let Some(p) = a
                    .value
                    .split('@')
                    .nth(1)
                    .filter(|_| a.value.starts_with('@') || a.value.contains("=@"))
                {
                    v.reads.push(Target {
                        path: p.to_string(),
                        dynamic: a.dynamic,
                        glob: false,
                        bound: false,
                        known: a
                            .known
                            .as_deref()
                            .and_then(|k| k.split('@').nth(1))
                            .map(str::to_string),
                    });
                }
            }
        }
        "wget" => {
            v.writes.extend(
                opt_value(args, Some('O'), &["output-document"])
                    .into_iter()
                    .map(|a| strip_opt_prefix(a, 'O', &["output-document"])),
            );
            v.writes.extend(
                opt_value(args, Some('P'), &["directory-prefix"])
                    .into_iter()
                    .map(|a| strip_opt_prefix(a, 'P', &["directory-prefix"])),
            );
        }
        "scp" | "rsync" => {
            let ops = operands_skipping(
                args,
                &[
                    "-e",
                    "-P",
                    "-i",
                    "-o",
                    "-F",
                    "-J",
                    "--rsh",
                    "--exclude",
                    "--include",
                    "--filter",
                ],
            );
            if let Some((dst, srcs)) = ops.split_last() {
                if !dst.value.contains(':') {
                    v.writes.push(Target::of(dst));
                }
                for s in srcs {
                    if !s.value.contains(':') {
                        v.reads.push(Target::of(s));
                    }
                }
            }
            if name == "rsync"
                && has_flag(
                    args,
                    &[],
                    &[
                        "delete",
                        "delete-before",
                        "delete-after",
                        "delete-during",
                        "remove-source-files",
                    ],
                )
            {
                v.risk = Risk::Dangerous;
                v.reason = "rsync deletes files (--delete)".into();
            }
        }
        "ssh" | "mosh" | "sshpass" => {
            v.reason = format!("opens a remote shell ({name})");
        }
        _ => {}
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<Arg> {
        v.iter().map(|s| Arg::lit(s)).collect()
    }

    #[test]
    fn flags_and_operands() {
        let args = a(&["-rf", "--verbose", "x", "--", "-y"]);
        assert!(has_flag(&args, &['r'], &[]));
        assert!(has_flag(&args, &['f'], &[]));
        assert!(!has_flag(&args, &['i'], &[]));
        assert!(has_flag(&args, &[], &["verbose"]));
        let ops: Vec<&str> = operands(&args).iter().map(|a| a.value.as_str()).collect();
        assert_eq!(ops, vec!["x", "-y"]);
        let args = a(&["-o", "out.txt", "--output=b"]);
        let o = opt_value(&args, Some('o'), &["output"]);
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn disk_devices() {
        assert!(is_disk_device("/dev/sda"));
        assert!(is_disk_device("/dev/nvme0n1"));
        assert!(!is_disk_device("/tmp/disk.img"));
        assert!(!is_disk_device("disk.img"));
    }
}
