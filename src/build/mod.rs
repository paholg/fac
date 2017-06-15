//! Rules and types for building the stuff.

#[cfg(test)]
extern crate quickcheck;
extern crate num_cpus;
extern crate bigbro;

use std;
use std::io;

use std::ffi::{OsString, OsStr};
use std::path::{Path,PathBuf};

use std::collections::{HashSet, HashMap};

use std::io::{Read, Write};

use git;
use ctrlc;
use notify;
use notify::{Watcher};

pub mod hashstat;
pub mod flags;
pub mod env;

/// VERBOSITY is used to enable our vprintln macro to know the
/// verbosity.  This is a bit ugly, but is needed due to rust macros
/// being hygienic.
static mut VERBOSITY: u64 = 0;

/// The `vprintln!` macro does a println! only if the --verbose flag
/// is specified.  It is written as a macro because if it were a
/// method or function then the arguments would be always evaluated
/// regardless of the verbosity (thus slowing things down).
/// Equivalently, we could write an if statement for each verbose
/// print, but that would be tedious.
macro_rules! vprintln {
    () => {{ if unsafe { VERBOSITY > 0 } { println!() } }};
    ($fmt:expr) => {{ if unsafe { VERBOSITY > 0 } { println!($fmt) } }};
    ($fmt:expr, $($arg:tt)*) => {{ if unsafe { VERBOSITY > 0 } { println!($fmt, $($arg)*) } }};
}

macro_rules! vvprintln {
    () => {{ if unsafe { VERBOSITY > 1 } { println!() } }};
    ($fmt:expr) => {{ if unsafe { VERBOSITY > 1 } { println!($fmt) } }};
    ($fmt:expr, $($arg:tt)*) => {{ if unsafe { VERBOSITY > 1 } { println!($fmt, $($arg)*) } }};
}

macro_rules! vvvprintln {
    () => {{ if unsafe { VERBOSITY > 2 } { println!() } }};
    ($fmt:expr) => {{ if unsafe { VERBOSITY > 2 } { println!($fmt) } }};
    ($fmt:expr, $($arg:tt)*) => {{ if unsafe { VERBOSITY > 2 } { println!($fmt, $($arg)*) } }};
}

/// `Id` is a type that should be unique to each Build.  This was
/// originally enforced by the type system using a unique lifetime,
/// but that ran me into trouble (and was verbose), so I dropped it.
/// Safety is now on the honor system, or rather based on the idea
/// that
///
/// 1. We must be very careful never to create an invalid RuleRef or
///    FileRef.
///
/// 2. We must never use a FileRef or RuleRef generated for one Build
///    in a different Build.
#[derive(Copy, Clone, PartialEq, PartialOrd, Eq, Hash)]
struct Id { id: std::marker::PhantomData<()>, }
impl Default for Id {
    #[inline]
    fn default() -> Self {
        Id { id: std::marker::PhantomData }
    }
}
impl std::fmt::Debug for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Id")
    }
}

/// The status of a rule.
#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
pub enum Status {
    /// This is the default status.
    Unknown,
    /// We are still deciding whether this needs to be built.
    BeingDetermined,
    /// We have determined that the rule doesn't need to run.
    Clean,
    /// The rule already ran.
    Built,
    /// The rule is currently being built.
    Building,
    /// The rule failed.
    Failed,
    /// This is used to indicate that specific rules are requested to
    /// be built.
    Marked,
    /// A rule cannot yet be built, because one of its inputs still
    /// needs to be built.
    Unready,
    /// This rule needs to be run.
    Dirty,
}

impl Status {
    /// This thing has been built or found to be clean
    pub fn is_done(self) -> bool {
        self == Status::Clean || self == Status::Built
    }
}

#[derive(PartialEq, Eq, Hash, Debug)]
struct StatusMap<T>( [T;9] );

impl<T> StatusMap<T> {
    fn new<F>(v: F) -> StatusMap<T>
        where F: Fn() -> T
    {
        StatusMap( [v(),v(),v(),v(),v(),v(),v(),v(),v()] )
    }
    #[cfg(test)]
    fn from(a: [T;9]) -> StatusMap<T> {
        StatusMap(a)
    }
}

impl<T: Clone> Clone for StatusMap<T> {
    fn clone(&self) -> Self {
        StatusMap( [self.0[0].clone(),
                    self.0[1].clone(),
                    self.0[2].clone(),
                    self.0[3].clone(),
                    self.0[4].clone(),
                    self.0[5].clone(),
                    self.0[6].clone(),
                    self.0[7].clone(),
                    self.0[8].clone()])
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for Status {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> Status {
        let choice: u32 = g.gen();
        match choice % 9 {
            0 => Status::Unknown,
            1 => Status::BeingDetermined,
            2 => Status::Clean,
            3 => Status::Built,
            4 => Status::Building,
            5 => Status::Failed,
            6 => Status::Marked,
            7 => Status::Unready,
            8 => Status::Dirty,
            _ => unimplemented!(),
        }
    }
}

#[cfg(test)]
impl<A: quickcheck::Arbitrary> quickcheck::Arbitrary for StatusMap<A> {
    fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> StatusMap<A> {
        StatusMap::from([quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
                         quickcheck::Arbitrary::arbitrary(g),
        ])
    }
}

#[cfg(test)]
quickcheck! {
    fn prop_can_access_statusmap(m: StatusMap<bool>, s: Status) -> bool {
        m[s] || !m[s]
    }
}

#[cfg(test)]
quickcheck! {
    fn prop_status_eq(s: Status) -> bool {
        s == s
    }
}

impl<T> std::ops::Index<Status> for StatusMap<T>  {
    type Output = T;
    fn index(&self, s: Status) -> &T {
        &self.0[s as usize]
    }
}
impl<T> std::ops::IndexMut<Status> for StatusMap<T>  {
    fn index_mut(&mut self, s: Status) -> &mut T {
        &mut self.0[s as usize]
    }
}

/// Is the file a regular file, a symlink, or a directory?
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone, Debug)]
pub enum FileKind {
    /// It is a regular file
    File,
    /// It is a directory
    Dir,
    /// It is a symlink
    Symlink,
}

/// A reference to a File
#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
pub struct FileRef(usize, Id);

/// A file (or directory) that is either an input or an output for
/// some rule.
#[derive(Debug)]
pub struct File {
    #[allow(dead_code)]
    id: Id,
    rule: Option<RuleRef>,
    path: PathBuf,
    // Question: could Vec be more efficient than RefSet here? It
    // depends if we add a rule multiple times to the same set of
    // children.  FIXME check this!
    children: HashSet<RuleRef>,

    rules_defined: Option<HashSet<RuleRef>>,

    hashstat: hashstat::HashStat,
    is_in_git: bool,
}

impl File {
    /// Set file properties...
    pub fn stat(&mut self) -> io::Result<FileKind> {
        let p = self.path.clone(); // FIXME UGLY workaround for borrow checker!
        self.hashstat.stat(&p)?;
        match self.hashstat.kind {
            Some(k) => Ok(k),
            None => Err(io::Error::new(io::ErrorKind::Other, "irregular file")),
        }
    }

    /// Is this `File` in git?
    pub fn in_git(&self) -> bool {
        self.is_in_git
    }

    /// Is this a fac file?
    pub fn is_fac_file(&self) -> bool {
        self.path.as_os_str().to_string_lossy().ends_with(".fac")
    }

    /// Does this thing exist?
    pub fn exists(&mut self) -> bool {
        if let Ok(h) = hashstat::stat(&self.path) {
            if h.size != self.hashstat.size || h.time != self.hashstat.time {
                self.hashstat = h;
            }
            true
        } else {
            false
        }
    }

    fn is_file(&mut self) -> bool {
        self.stat().ok();
        self.hashstat.kind == Some(FileKind::File)
    }
    fn is_dir(&mut self) -> bool {
        self.stat().ok();
        self.hashstat.kind == Some(FileKind::Dir)
    }

    /// Remove it if it exists.
    fn unlink(&self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// A reference to a Rule
#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
pub struct RuleRef(usize, Id);
unsafe impl Send for RuleRef {}

/// A rule for building something.
#[derive(Debug)]
pub struct Rule {
    #[allow(dead_code)]
    id: Id,
    inputs: Vec<FileRef>,
    outputs: Vec<FileRef>,
    all_inputs: HashSet<FileRef>,
    all_outputs: HashSet<FileRef>,
    hashstats: HashMap<FileRef, hashstat::HashStat>,

    status: Status,
    cache_prefixes: HashSet<OsString>,
    cache_suffixes: HashSet<OsString>,

    working_directory: PathBuf,
    facfile: FileRef,
    linenum: usize,
    command: OsString,
    is_default: bool,
}

impl Rule {
    /// Identifies whether a given path is "cache"
    pub fn is_cache(&self, path: &Path) -> bool {
        self.cache_suffixes.iter().any(|s| is_suffix(path, s)) ||
            self.cache_prefixes.iter().any(|s| is_prefix(path, s))
    }
}

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt};
#[cfg(unix)]
fn is_suffix(path: &Path, suff: &OsStr) -> bool {
    let l = suff.as_bytes().len();
    let pp = path.as_os_str().as_bytes().len();
    pp > l && path.as_os_str().as_bytes()[pp-l..] == suff.as_bytes()[..]
}
#[cfg(unix)]
fn is_prefix(path: &Path, suff: &OsStr) -> bool {
    let l = suff.as_bytes().len();
    let p = path.as_os_str().as_bytes();
    p.len() > l && p[..l] == suff.as_bytes()[..]
}
#[cfg(unix)]
fn cut_suffix(path: &Path, suff: &OsStr) -> Option<PathBuf> {
    let l = suff.as_bytes().len();
    if let Some(fname) = path.file_name() {
        let fname = fname.as_bytes();
        let pp = fname.len();
        if pp > l && fname[pp-l..] == suff.as_bytes()[..] {
            Some(path.with_file_name(bytes_to_osstr(&fname[..pp-l])))
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(not(unix))]
fn is_suffix(path: &Path, suff: &OsStr) -> bool {
    let pathstring: String = path.as_os_str().to_string_lossy().into_owned();
    let suffstring: String = suff.to_string_lossy().into_owned();
    pathstring.ends_with(&suffstring)
}
#[cfg(not(unix))]
fn is_prefix(path: &Path, suff: &OsStr) -> bool {
    let pathstring: String = path.as_os_str().to_string_lossy().into_owned();
    let suffstring: String = suff.to_string_lossy().into_owned();
    pathstring.starts_with(&suffstring)
}
#[cfg(not(unix))]
fn cut_suffix(path: &Path, suff: &OsStr) -> Option<PathBuf> {
    if let Some(fname) = path.file_name() {
        let fnamestring: String = fname.to_string_lossy().into_owned();
        let suffstring: String = suff.to_string_lossy().into_owned();
        if fnamestring.ends_with(&suffstring) {
            Some(path.with_file_name(OsStr::new(fnamestring.trim_right_matches(&suffstring))))
        } else {
            None
        }
    } else {
        None
    }
}

#[test]
fn test_is_prefix() {
    assert!(is_prefix(std::path::Path::new("/the/world/is"),
                      std::ffi::OsStr::new("/the/world")));
    assert!(!is_prefix(std::path::Path::new("/the/world/is"),
                       std::ffi::OsStr::new("/the/world/other")));
}

#[test]
fn test_is_suffix() {
    assert!(is_suffix(std::path::Path::new("/the/world/is.awesome"),
                      std::ffi::OsStr::new("awesome")));
    assert!(!is_suffix(std::path::Path::new("/the/world/is"),
                       std::ffi::OsStr::new("/the/world")));
}

#[derive(Debug)]
enum Event {
    Finished(RuleRef, io::Result<bigbro::Status>),
    NotifyChange(PathBuf),
    CtrlC,
}
unsafe impl Send for Event {}

/// A struct that holds all the information needed to build.  You can
/// think of this as behaving like a set of global variables, but we
/// can drop the whole thing.
///
/// Build is implmented using a type witness `ID`, which makes it
/// impossible to create RuleRefs and FileRefs that are out of bounds.
/// I really should put some of these things in a "private" module so
/// that I can provide stronger guarantees of correctness in the main
/// build module.
#[derive(Debug)]
pub struct Build {
    #[allow(dead_code)]
    id: Id,
    files: Vec<File>,
    rules: Vec<Rule>,
    filemap: HashMap<PathBuf, FileRef>,
    rulemap: HashMap<(OsString, PathBuf), RuleRef>,
    statuses: StatusMap<HashSet<RuleRef>>,
    /// The set of rules with Status::Marked, topologically sorted
    /// such that we could build them in this order.  We use this to
    /// avoid a stack overflow when checking for cleanliness, in case
    /// where there is a long dependency chain (> 1k long).  Note that
    /// marked_rules may contain duplicates, as well as rules that are
    /// no longer marked.
    marked_rules: Vec<RuleRef>,

    facfiles_used: HashSet<FileRef>,

    recv_rule_status: std::sync::mpsc::Receiver<Event>,
    send_rule_status: std::sync::mpsc::Sender<Event>,
    process_killers: HashMap<RuleRef, bigbro::Killer>,
    am_interrupted: bool,

    flags: flags::Flags,
}

/// Construct a new `Build` and use it to build.
pub fn build(fl: flags::Flags) -> i32 {
    let (tx,rx) = std::sync::mpsc::channel();
    unsafe { VERBOSITY = fl.verbosity; }
    // This approach to type witnesses is taken from
    // https://github.com/bluss/indexing/blob/master/src/container.rs
    let mut b = Build {
        id: Id::default(),
        files: Vec::new(),
        rules: Vec::new(),
        filemap: HashMap::new(),
        rulemap: HashMap::new(),
        statuses: StatusMap::new(|| HashSet::new()),
        marked_rules: Vec::new(),
        facfiles_used: HashSet::new(),
        recv_rule_status: rx,
        send_rule_status: tx,
        process_killers: HashMap::new(),
        am_interrupted: false,
        flags: fl,
    };
    for ref f in git::ls_files() {
        b.new_file_private(f, true);
    }
    b.build()
}

impl Build {
    /// Run the actual build!
    pub fn build(&mut self) -> i32 {
        if self.flags.parse_only.is_some() {
            let p = self.flags.run_from_directory.join(self.flags.parse_only
                                                       .as_ref().unwrap());
            let fr = self.new_file(&p);
            if let Err(e) = self.read_facfile(fr) {
                println!("{}", e);
                return 1;
            }
            println!("finished parsing file {:?}", self.pretty_path_peek(fr));
            return 0;
        }
        if self.flags.jobs == 0 {
            self.flags.jobs = num_cpus::get();
            vprintln!("Using {} jobs", self.flags.jobs);
        }
        let ctrl_c_sender = self.send_rule_status.clone();
        ctrlc::set_handler(move || {
            ctrl_c_sender.send(Event::CtrlC).expect("Error reporting Ctrl-C");
            print!("... ");
        }).expect("Error setting Ctrl-C handler");

        // Start by reading any facfiles in the git repository.
        for f in self.filerefs() {
            if self[f].is_fac_file() {
                if let Err(e) = self.read_facfile(f) {
                    println!("{}", e);
                    self.unlock_repository_and_exit(1);
                }
            }
        }
        if self.rulerefs().len() == 0 {
            println!("Please git add a .fac file containing rules!");
            self.unlock_repository_and_exit(1);
        }

        self.lock_repository();

        // First build the facfiles, which should already be marked,
        // and should get marked as we go.
        self.build_dirty();

        if self.flags.clean {
            for o in self.filerefs() {
                if self.is_git_path(&self[o].path) {
                    continue; // do not want to clean git paths!
                }
                if self.recv_rule_status.try_recv().is_ok() {
                    println!("Interrupted!");
                    self.emergency_unlock_repository().expect("trouble removing lock file");
                    std::process::exit(1);
                }
                if !self[o].is_in_git && self[o].is_file() && self[o].rule.is_some() {
                    vprintln!("rm {:?}", self.pretty_path_peek(o));
                    self[o].unlink();
                }
                if self[o].is_fac_file() {
                    let factum = self[o].path.with_extension("fac.tum");
                    std::fs::remove_file(&factum).ok();
                    vprintln!("rm {:?}", &factum);
                }
            }
            // The following bit is a hokey and inefficient bit of
            // code to ensure that we will rmdir subdirectories prior
            // to their superdirectories.  I don't bother checking if
            // anything is a directory or not, and I recompute depths
            // many times.
            let mut dirs: Vec<FileRef> = self.filerefs().iter()
                .map(|&o| o)
                .filter(|&o| self[o].is_dir()//  && self[o].rule.is_some()
                ).collect();
            dirs.sort_by_key(|&d| - (self[d].path.to_string_lossy().len() as i32));
            for d in dirs {
                if self.is_git_path(&self[d].path) {
                    continue; // do not want to clean git paths!
                }
                vprintln!("rmdir {:?}", self.pretty_path_peek(d));
                std::fs::remove_dir(&self[d].path).ok();
            }
            self.unlock_repository_and_exit(0);
        }

        let mut first_time_through = true;
        while first_time_through || self.flags.continual {
            if !first_time_through {
                self.lock_repository();
            }
            first_time_through = false;

            // Now we start building the actual targets.
            self.mark_all();
            self.build_dirty();

            self.unlock_repository().unwrap();
            let result = self.summarize_build_results();
            if result != 0 && !self.flags.continual {
                return result;
            }
            if result == 0 &&
                (self.flags.makefile.is_some() || self.flags.ninja.is_some()
                 || self.flags.tupfile.is_some() || self.flags.script.is_some()
                 || self.flags.tar.is_some())
            {
                for r in self.rulerefs() {
                    self.set_status(r, Status::Unknown);
                }
                self.mark_all();
                if let Some(ref f) = self.flags.script {
                    let mut f = std::fs::File::create(f).unwrap();
                    self.write_script(&mut f).unwrap();
                }
                if let Some(ref mf) = self.flags.makefile {
                    let mut f = std::fs::File::create(mf).unwrap();
                    self.write_makefile(&mut f).unwrap();
                }
                if let Some(ref f) = self.flags.ninja {
                    let mut f = std::fs::File::create(f).unwrap();
                    self.write_ninja(&mut f).unwrap();
                }
                if let Some(ref f) = self.flags.tupfile {
                    let mut f = std::fs::File::create(f).unwrap();
                    self.write_tupfile(&mut f).unwrap();
                }
                if let Some(ref f) = self.flags.tar {
                    self.create_tarball(f).unwrap();
                }
            }
            if self.flags.continual {
                println!("I am going to do a continual thingy");
                let (notify_tx, notify_rx) = std::sync::mpsc::channel();
                let mut watcher =
                    notify::watcher(notify_tx,
                                    std::time::Duration::from_secs(3)).unwrap();
                let srs = self.send_rule_status.clone();
                std::thread::spawn(move || {
                    loop {
                        match notify_rx.recv().unwrap() {
                            notify::DebouncedEvent::Write(fname) => {
                                println!("Write event in {:?}", &fname);
                                if let Some(dname) = fname.parent() {
                                    srs.send(Event::NotifyChange(PathBuf::from(dname))).unwrap();
                                }
                                srs.send(Event::NotifyChange(fname)).unwrap();
                            },
                            notify::DebouncedEvent::Rename(fname, newname) => {
                                println!("Rename event {:?} -> {:?}", &fname, &newname);
                                if let Some(dname) = fname.parent() {
                                    srs.send(Event::NotifyChange(PathBuf::from(dname))).unwrap();
                                }
                                if let Some(dname) = newname.parent() {
                                    srs.send(Event::NotifyChange(PathBuf::from(dname))).unwrap();
                                }
                                srs.send(Event::NotifyChange(fname)).unwrap();
                                srs.send(Event::NotifyChange(newname)).unwrap();
                            },
                            _ => (),
                        }
                    }
                });

                for f in self.filerefs() {
                    // Avoid watching system directories, since this
                    // can lead to significant and pointless rechecking.
                    if self[f].children.len() > 0
                        && (self.is_local(f) || self[f].is_file()) {
                        watcher.watch(&self[f].path,
                                      notify::RecursiveMode::NonRecursive).ok();
                    }
                }
                println!("I am about to match...");
                let mut found_something = false;
                while !found_something {
                    match self.recv_rule_status.recv() {
                        Ok(Event::Finished(_,_)) => unreachable!(),
                        Ok(Event::NotifyChange(fname)) => {
                            println!("{:?} changed, rebuilding...", fname);
                            let t = self.new_file(&fname);
                            if self[t].children.len() == 0 {
                                println!("Disabling watcher on {:?}", &fname);
                                if let Err(e) = watcher.unwatch(fname) {
                                    println!("Error disabling: {:?}", e);
                                }
                                continue;
                            }
                            found_something = true;
                            for &c in self[t].children.iter() {
                                println!("It affects rule {}", self.pretty_rule(c));
                            }
                            self[t].exists(); // clear out the hash!
                            self.modified_file(t);
                            while let Ok(msg) = self.recv_rule_status.try_recv() {
                                match msg {
                                    Event::Finished(_,_) => unreachable!(),
                                    Event::NotifyChange(fname) => {
                                        println!("{:?} changed, rebuilding...", fname);
                                        let t = self.new_file(fname);
                                        self[t].exists(); // clear out the hash!
                                        self.modified_file(t);
                                    },
                                    Event::CtrlC => {
                                        self.am_interrupted = true;
                                        println!("Interrupted!");
                                        self.unlock_repository_and_exit(1);
                                    },
                                }
                            }
                        },
                        Err(e) => {
                            println!("error receiving status: {}", e);
                            self.am_interrupted = true;
                            self.unlock_repository_and_exit(1);
                        },
                        Ok(Event::CtrlC) => {
                            self.am_interrupted = true;
                            println!("Interrupted!");
                            self.unlock_repository_and_exit(1);
                        },
                    }
                    println!("I finished match... {} dirty",
                             self.statuses[Status::Dirty].len());
                }
            }
        }
        0
    }
    /// Build the dirty rules
    fn build_dirty(&mut self) {
        while self.statuses[Status::Dirty].len() > 0
            || self.statuses[Status::Unready].len() > 0
            || self.statuses[Status::Marked].len() > 0
            || self.num_building() > 0
        {
            vvvprintln!("   {} unknown", self.statuses[Status::Unknown].len());
            vvvprintln!("   {} clean", self.statuses[Status::Clean].len());
            vvvprintln!("   {} marked", self.statuses[Status::Marked].len());
            vvvprintln!("   {} built", self.statuses[Status::Built].len());
            vvvprintln!("   {} failed", self.statuses[Status::Failed].len());
            vvvprintln!("   {} building", self.statuses[Status::Building].len());
            let rules: Vec<_> = std::mem::replace(&mut self.marked_rules, Vec::new());
            for r in rules {
                self.check_cleanliness(r);
                if self.rule(r).status == Status::Clean {
                    // Mark for checking the children of this rule, as
                    // they may now be buildable.
                    let children: HashSet<_> = self.rule(r).all_outputs.iter()
                        .flat_map(|&o| self[o].children.iter()).map(|&c| c).collect();
                    for c in children {
                        self.definitely_mark(c);
                    }
                }
            }

            if self.num_building() < self.flags.jobs {
                let rules: Vec<_> = self.statuses[Status::Dirty].iter()
                    .take(self.flags.jobs - self.num_building())
                    .map(|&r| r).collect();
                for r in rules {
                    if let Err(e) = self.spawn(r) {
                        println!("I got err {}", e);
                    }
                }
            }
            if self.num_building() > 0 {
                self.wait_for_a_rule();
            }

            if self.statuses[Status::Dirty].len() == 0
                && self.statuses[Status::Marked].len() == 0
                && self.num_building() == 0
                && self.statuses[Status::Unready].len() > 0
            {
                // Looks like we failed to build everything! There are
                // a few possibilities, including the possibility that
                // one of these unready rules is actually ready after
                // all.
                let rules: Vec<_>
                    = self.statuses[Status::Unready].iter().map(|&r| r).collect();
                for r in rules {
                    let mut need_to_try_again = false;
                    let mut have_explained_failure = false;
                    // FIXME figure out ignore_missing_files,
                    // happy_building_at_least_one, etc. from
                    // build.c:1171
                    let inputs: Vec<_> = self.rule(r).all_inputs.iter()
                        .map(|&i| i).collect();
                    for i in inputs {
                        if self[i].rule.is_none() && !self[i].is_in_git &&
                            !self.is_git_path(&self[i].path) &&
                            self[i].path.starts_with(&self.flags.root)
                        {
                            if self[i].exists() {
                                let thepath = self[i].path.canonicalize().unwrap();
                                if thepath != self[i].path {
                                    // The canonicalization of the
                                    // path has changed! See issue #17
                                    // which this fixes. Presumably a
                                    // directory has been created or a
                                    // symlink modified, and the path
                                    // is now different.
                                    let t = self.new_file(thepath);
                                    // There is a small memory leak
                                    // here, since we don't free the
                                    // old target.  The trouble is
                                    // that we don't know if it is
                                    // otherwise in use, e.g. as the
                                    // output or input of a different
                                    // rule.  This *shouldn't* be
                                    // common, since once the path
                                    // exists, future runs will not
                                    // run into this leak.
                                    self.rule_mut(r).all_inputs.remove(&i);
                                    self.rule_mut(r).all_inputs.insert(t);
                                    self[i].children.remove(&r);
                                    self[t].children.insert(r);
                                    // Now replace the target in the
                                    // vec of explicit inputs.
                                    for ii in self.rule_mut(r).inputs.iter_mut() {
                                        if *ii == i {
                                            *ii = t;
                                        }
                                    }
                                    need_to_try_again = true;
                                    // } else if git_add_files {
                                    //     git_add(r->inputs[i]->path);
                                    //     need_to_try_again = true;
                                } else {
                                    have_explained_failure = true;
                                    println!("error: add {:?} to git, which is required for {}",
                                             self.pretty_path_peek(i),
                                             self.pretty_reason(r));
                                }
                            } else {
                                have_explained_failure = true;
                                println!("error: missing file {:?}, which is required for {}",
                                         self.pretty_path_peek(i), self.pretty_reason(r));
                            }
                        }
                    }
                    if need_to_try_again {
                        self.definitely_mark(r);
                        break;
                    } else if have_explained_failure {
                        self.failed(r);
                    } else {
                        println!("weird failure: {} {:?}", self.pretty_rule(r),
                                 self.rule(r).status);
                        self.set_status(r, Status::Unknown);
                        self.check_cleanliness(r);
                        println!("    actual cleanliness of {} is {:?}", self.pretty_rule(r),
                                 self.rule(r).status);
                        self.failed(r);
                    }
                }
            }
        }
    }
    /// either take a lock on the repository, or exit
    fn lock_path(&self) -> PathBuf {
        git::git_dir().join("fac-lock")
    }
    /// either take a lock on the repository, or exit
    fn lock_repository(&self) {
        let fname = self.lock_path();
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&fname).is_err() {
            for _ in [0..10].iter() {
                println!("fac is already running... sleeping a bit");
                std::thread::sleep(std::time::Duration::from_secs(1));
                if std::fs::OpenOptions::new().write(true).create_new(true)
                    .open(&fname).is_ok()
                {
                    return;
                }
            }
            println!("Giving up after 10 seconds... remove .git/fac-lock?");
            std::process::exit(1);
        }
    }
    /// unlock_repository saves any factum files and also removes the
    /// lock file.
    fn unlock_repository(&mut self) -> std::io::Result<()> {
        while self.num_building() > 0 {
            println!("I have {} processes to halt...", self.num_building());
            for mut k in self.process_killers.values_mut() {
                k.terminate().ok();
            }
            // give processes a second to die...
            std::thread::sleep(std::time::Duration::from_secs(1));
            while self.check_for_a_rule() {
                // nothing to do here?
            }
            if self.num_building() > 0 {
                println!("I have {} processes that were stubborn and need more force...",
                         self.num_building());
                for mut k in self.process_killers.values_mut() {
                    k.kill().ok();
                }
                // give processes a second to die...
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            while self.check_for_a_rule() {
                // nothing to do here?
            }
        }
        let e1 = self.save_factum_files();
        let e2 = self.emergency_unlock_repository();
        if e1.is_err() {
            e1
        } else {
            e2
        }
    }
    /// remove the lock file without doing anything else (e.g. saving
    /// facfiles, or killing child processes)!
    fn emergency_unlock_repository(&mut self) -> std::io::Result<()> {
        std::fs::remove_file(self.lock_path())
    }
    fn unlock_repository_and_exit(&mut self, exitcode: i32) {
        self.unlock_repository().ok();
        std::process::exit(exitcode);
    }
    fn filerefs(&self) -> Vec<FileRef> {
        let mut out = Vec::new();
        for i in 0 .. self.files.len() {
            out.push(FileRef(i, self.id));
        }
        out
    }
    fn rulerefs(&self) -> Vec<RuleRef> {
        let mut out = Vec::new();
        for i in 0 .. self.rules.len() {
            out.push(RuleRef(i, self.id));
        }
        out
    }
    fn new_file_private<P: AsRef<Path>>(&mut self, path: P,
                                        is_in_git: bool)
                                        -> FileRef {
        let path = self.flags.root.join(path.as_ref());
        // If this file is already in our database, use the version
        // that we have.  It is an important invariant that we can
        // only have one file with a given path in the database.
        if let Some(&f) = self.filemap.get(&path) {
            return f;
        }
        let f = FileRef(self.files.len(), self.id);
        self.files.push(File {
            id: self.id,
            rule: None,
            path: path.clone(),
            children: HashSet::new(),
            rules_defined: None,
            hashstat: hashstat::HashStat::empty(),
            is_in_git: is_in_git,
        });
        if is_in_git {
            if let Some(parent) = path.parent() {
                if parent.starts_with(&self.flags.root) {
                    // if child is in git, then parent must also be in
                    // git!
                    self.new_file_private(parent, is_in_git);
                }
            }
        }
        self.filemap.insert(path, f);
        f
    }

    /// Look up a `File` corresponding to a path, or if it doesn't
    /// exist, allocate space for a new `File`.
    pub fn new_file<P: AsRef<Path>>(&mut self, path: P) -> FileRef {
        self.new_file_private(path, false)
    }

    /// Allocate space for a new `Rule`.
    pub fn new_rule(&mut self,
                    command: &OsStr,
                    working_directory: &Path,
                    facfile: FileRef,
                    linenum: usize,
                    cache_suffixes: HashSet<OsString>,
                    cache_prefixes: HashSet<OsString>,
                    is_default: bool)
                    -> Result<RuleRef, RuleRef> {
        let r = RuleRef(self.rules.len(), self.id);
        let key = (OsString::from(command), PathBuf::from(working_directory));
        if self.rulemap.contains_key(&key) {
            return Err(self.rulemap[&key]);
        }
        self.rulemap.insert(key, r);
        self.rules.push(Rule {
            id: self.id,
            inputs: Vec::new(),
            outputs: Vec::new(),
            all_inputs: HashSet::new(),
            all_outputs: HashSet::new(),
            hashstats: HashMap::new(),
            status: Status::Unknown,
            cache_prefixes: cache_prefixes,
            cache_suffixes: cache_suffixes,
            working_directory: PathBuf::from(working_directory),
            facfile: facfile,
            linenum: linenum,
            command: OsString::from(command),
            is_default: is_default,
        });
        self.statuses[Status::Unknown].insert(r);
        Ok(r)
    }

    /// Read a fac file
    pub fn read_facfile(&mut self, fileref: FileRef) -> io::Result<()> {
        self[fileref].rules_defined = Some(HashSet::new());
        let filepath = self[fileref].path.clone();
        let fp = self.pretty_path_peek(fileref).to_string_lossy().into_owned();
        let mut f = match std::fs::File::open(&filepath) {
            Ok(f) => f,
            Err(e) =>
                return Err(io::Error::new(e.kind(),
                                          format!("error: unable to open file {:?}: {}",
                                                  self.pretty_path_peek(fileref), e))),
        };
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        let mut command: Option<RuleRef> = None;
        for (lineno_minus_one, line) in v.split(|c| *c == b'\n').enumerate() {
            let lineno = lineno_minus_one + 1;
            if line.len() < 2 || line[0] == b'#' { continue };
            if line[1] != b' ' {
                return Err(
                    io::Error::new(io::ErrorKind::Other,
                                   format!("error: {}:{}: {}", &fp, lineno,
                                           "Second character of line should be a space.")));
            }
            let get_rule = |r: Option<RuleRef>, c: char| -> io::Result<RuleRef> {
                match r {
                    None =>
                        return Err(
                            io::Error::new(
                                io::ErrorKind::Other,
                                format!("error: {}:{}: {}", &fp, lineno,
                                        &format!("'{}' line must follow '|' or '?'",c)))),
                    Some(r) => Ok(r),
                }
            };
            match line[0] {
                b'|' => {
                    match self.new_rule(bytes_to_osstr(&line[2..]),
                                        filepath.parent().unwrap(),
                                        fileref,
                                        lineno,
                                        HashSet::new(),
                                        HashSet::new(),
                                        true) {
                        Ok(r) => {
                            self[fileref].rules_defined
                                .as_mut().expect("rules_defined should be some!").insert(r);
                            command = Some(r);
                        },
                        Err(e) => {
                            return Err(
                                io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("error: {}:{} duplicate rule: {}\n\talso defined in {}:{}",
                                            &fp, lineno, self.pretty_rule(e),
                                            self.pretty_path_peek(self.rule(e).facfile).display(),
                                            self.rule(e).linenum)));
                        },
                    }
                },
                b'?' => {
                    match self.new_rule(bytes_to_osstr(&line[2..]),
                                        filepath.parent().unwrap(),
                                        fileref,
                                        lineno,
                                        HashSet::new(),
                                        HashSet::new(),
                                        false) {
                        Ok(r) => {
                            self[fileref].rules_defined
                                .as_mut().expect("rules_defined should be some!").insert(r);
                            command = Some(r);
                        },
                        Err(e) => {
                            return Err(
                                io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("error: {}:{} duplicate rule: {}\n\talso defined in {}:{}",
                                            &fp, lineno, self.pretty_rule(e),
                                            self.pretty_path_peek(self.rule(e).facfile).to_string_lossy(),
                                            self.rule(e).linenum)));
                        },
                    }
                },
                b'>' => {
                    let f = self.new_file( &normalize(&filepath.parent().unwrap()
                                                      .join(bytes_to_osstr(&line[2..]))));
                    let r = get_rule(command, '>')?;
                    self.add_explicit_output(r, f);
                    if self[f].is_fac_file() {
                        // always mark any rules that generate explicit fac files
                        self.mark(r);
                    }
                },
                b'<' => {
                    let f = self.new_file( &normalize(&filepath.parent().unwrap()
                                                      .join(bytes_to_osstr(&line[2..]))));
                    self.add_explicit_input(get_rule(command, '<')?, f);
                },
                b'c' => {
                    self.rule_mut(get_rule(command, 'c')?).cache_suffixes
                        .insert(bytes_to_osstr(&line[2..]).to_os_string());
                },
                b'C' => {
                    let prefix = bytes_to_osstr(&line[2..]).to_os_string();
                    let prefix = if PathBuf::from(&prefix).is_absolute() {
                        prefix
                    } else {
                        self.flags.root.join(&prefix).into_os_string()
                    };
                    self.rule_mut(get_rule(command, 'C')?).cache_prefixes
                        .insert(prefix);
                },
                _ => {
                    return Err(
                        io::Error::new(
                            io::ErrorKind::Other,
                            format!("error: {}:{}: {}", &fp, lineno,
                                    &format!("Invalid first character: {:?}", line[0]))))
                },
            }
        }
        self.read_factum_file(fileref)
    }
    /// Read a factum file
    fn read_factum_file(&mut self, fileref: FileRef) -> io::Result<()> {
        let filepath = self[fileref].path.with_extension("fac.tum");
        let mut f = if let Ok(f) = std::fs::File::open(&filepath) {
            f
        } else {
            return Ok(()); // not an error for factum file to not exist!
        };
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        let mut command: Option<RuleRef> = None;
        let mut file: Option<FileRef> = None;
        for (lineno_minus_one, line) in v.split(|c| *c == b'\n').enumerate() {
            let lineno = lineno_minus_one + 1;
            fn parse_error<T>(path: &Path, lineno: usize, msg: &str) -> io::Result<T> {
                Err(io::Error::new(io::ErrorKind::Other,
                                        format!("error: {:?}:{}: {}",
                                                path, lineno, msg)))
            }
            if line.len() < 2 || line[0] == b'#' { continue };
            if line[1] != b' ' {
                return parse_error(&filepath, lineno,
                                   "Second character of line should be a space.");
            }
            match line[0] {
                b'|' => {
                    let key = (bytes_to_osstr(&line[2..]).to_os_string(),
                               PathBuf::from(filepath.parent().unwrap()));
                    if let Some(r) = self.rulemap.get(&key) {
                        if self[fileref].rules_defined
                            .as_ref().expect("rules_defined should be some!").contains(r)
                        {
                            command = Some(*r);
                            file = None;
                        } else {
                            println!("mystery rule moved to different fac file?!");
                        }
                    } else {
                        command = None;
                    }
                },
                b'>' => {
                    let f = self.new_file(bytes_to_osstr(&line[2..]));
                    file = Some(f);
                    if let Some(r) = command {
                        self.add_output(r, f);
                    } else {
                        if !self[f].is_in_git {
                            // looks like a stray output that deserves
                            // to be cleaned up before we forget about
                            // it!
                            self[f].unlink();
                        }
                    }
                },
                b'<' => {
                    let f = self.new_file(bytes_to_osstr(&line[2..]));
                    file = Some(f);
                    if let Some(r) = command {
                        self.add_input(r, f);
                    }
                },
                b'H' => {
                    if let Some(ff) = file {
                        if let Some(r) = command {
                            self.rule_mut(r)
                                .hashstats.insert(ff, hashstat::HashStat::decode(&line[2..]));
                        }
                    } else {
                        return parse_error(&filepath, lineno,
                                           &format!("H must be after a file!"));
                    }
                },
                _ => (),
            }
        }
        Ok(())
    }

    /// Write a fac file
    pub fn print_fac_file(&mut self, fileref: FileRef) -> io::Result<()> {
        if let Some(ref rules_defined) = self[fileref].rules_defined {
            for &r in rules_defined.iter() {
                println!("| {}", self.rule(r).command.to_string_lossy());
                for &i in self.rule(r).inputs.iter() {
                    println!("< {}", self[i].path.display());
                }
                for &o in self.rule(r).outputs.iter() {
                    println!("> {}", self[o].path.display());
                }
            }
        }
        Ok(())
    }
    /// Write factum files
    pub fn save_factum_files(&mut self) -> io::Result<()> {
        let facfiles: Vec<FileRef> = self.facfiles_used.drain().collect();
        for f in facfiles {
            self.save_factum_file(f).unwrap();
        }
        Ok(())
    }
    /// Write a fac.tum file
    pub fn save_factum_file(&mut self, fileref: FileRef) -> io::Result<()> {
        let f = std::fs::File::create(&self[fileref].path.with_extension("fac.tum"))?;
        let mut f = std::io::BufWriter::new(f);
        if let Some(ref rules_defined) = self[fileref].rules_defined {
            for &r in rules_defined.iter() {
                f.write(b"\n| ")?;
                f.write(hashstat::osstr_to_bytes(&self.rule(r).command))?;
                f.write(b"\n")?;
                for &i in self.rule(r).all_inputs.iter() {
                    f.write(b"< ")?;
                    f.write(hashstat::osstr_to_bytes(self.pretty_path(i).as_os_str()))?;
                    if let Some(st) = self.rule(r).hashstats.get(&i) {
                        f.write(b"\nH ")?;
                        f.write(&st.encode())?;
                    }
                    f.write(b"\n")?;
                }
                for &o in self.rule(r).all_outputs.iter() {
                    f.write(b"> ")?;
                    f.write(hashstat::osstr_to_bytes(self.pretty_path(o).as_os_str()))?;
                    f.write(b"\nH ")?;
                    f.write(&self[o].hashstat.encode())?;
                    f.write(b"\n")?;
                }
            }
        }
        Ok(())
    }

    fn create_tarball(&self, tarname: &PathBuf) -> io::Result<()> {
        let (dirname, flags) =
            if let Some(dirname) = cut_suffix(tarname, OsStr::new(".tgz")) {
                (dirname, "czf")
            } else if let Some(dirname) = cut_suffix(tarname, OsStr::new(".tar.gz")) {
                (dirname, "czf")
            } else if let Some(dirname) = cut_suffix(tarname, OsStr::new(".tar.bz2")) {
                (dirname, "cjf")
            } else if let Some(dirname) = cut_suffix(tarname, OsStr::new(".tar")) {
                (dirname, "cf")
            } else {
                return Err(io::Error::new(io::ErrorKind::Other,
                                          format!("invalid tar filename: {:?}", tarname)));
            };
        vprintln!("removing {:?}", &dirname);
        std::fs::remove_dir_all(&dirname).ok(); // ignore errors, which are probably does-not-exist
        std::fs::create_dir(&dirname)?;

        for &r in self.marked_rules.iter() {
            for i in self.rule(r).all_inputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].rule.is_none())
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
            {
                cp_to_dir(self.pretty_path_peek(i), &dirname).ok();
            }
        }
        for &option_path in &[&self.flags.script, &self.flags.makefile,
                              &self.flags.tupfile, &self.flags.ninja]
        {
            if let Some(ref f) = *option_path {
                cp_to_dir(f, &dirname)?;
            }
        }
        for p in self.flags.include_in_tar.iter() {
            cp_to_dir(p, &dirname)?;
        }
        let status = std::process::Command::new("tar")
            .arg(flags).arg(tarname).arg(&dirname).status()?;
        vprintln!("removing {:?} again", &dirname);
        std::fs::remove_dir_all(&dirname).ok();
        if !status.success() {
            Err(io::Error::new(io::ErrorKind::Other, format!("Error running tar!")))
        } else {
            Ok(())
        }
    }


    /// Output a makefile to do the build
    pub fn write_makefile<F: Write>(&self, f: &mut F) -> io::Result<()> {
        write!(f, "all:")?;
        let mut targets: Vec<_> = self.statuses[Status::Marked].iter()
            .filter(|&&r| self.rule(r).all_outputs.len() > 0
                    && self.rule(r).all_outputs.iter().all(|&o| self[o].children.len() == 0))
            .map(|&r| self.pretty_rule_output(r)).collect();
        targets.sort();
        for t in targets {
            write!(f, " {}", t.display())?;
        }
        writeln!(f, "\n")?;
        let mut rules: Vec<_> = self.statuses[Status::Marked].iter().map(|&r| r).collect();
        rules.sort_by_key(|&r| self.pretty_rule(r));

        write!(f, "clean:\n\trm -f")?;
        let mut clean_files: HashSet<PathBuf> = HashSet::new();
        for &r in rules.iter() {
            clean_files.extend(self.rule(r).all_outputs.iter()
                               .filter(|&o| self[*o].path.starts_with(&self.flags.root))
                               .filter(|&o| self[*o].hashstat.kind == Some(FileKind::File))
                               .map(|&o| self.pretty_path(o)));
        }
        let mut clean_files: Vec<_> = clean_files.iter().collect();
        clean_files.sort();
        for c in clean_files {
            write!(f, " {:?}", c)?;
        }
        writeln!(f)?;

        for r in rules {
            let mut inps: Vec<_> = self.rule(r).all_inputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
                .map(|i| self.pretty_path(i))
                .collect();
            let mut outs: Vec<_> = self.rule(r).all_outputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
                .map(|i| self.pretty_path(i))
                .collect();
            inps.sort();
            outs.sort();
            for o in outs {
                write!(f, "{} ", o.display())?;
            }
            write!(f, ":")?;
            for i in inps {
                write!(f, " {}", i.display())?;
            }
            if self.rule(r).working_directory == self.flags.root {
                writeln!(f, "\n\t{}", self.rule(r).command.to_string_lossy())?;
            } else {
                writeln!(f, "\n\tcd {:?} && {}",
                         self.rule(r).working_directory
                              .strip_prefix(&self.flags.root).unwrap(),
                         self.rule(r).command.to_string_lossy())?;
            }
        }
        Ok(())
    }

    /// Output a script to do the build
    pub fn write_script<F: Write>(&self, f: &mut F) -> io::Result<()> {
        f.write(b"#!/bin/sh\n\nset -ev\n\n")?;
        for &r in &self.marked_rules {
            if self.rule(r).working_directory != self.flags.root {
                writeln!(f, "(cd {:?} && {})",
                         self.rule(r).working_directory
                              .strip_prefix(&self.flags.root).unwrap(),
                         self.rule(r).command.to_string_lossy())?;
            } else {
                writeln!(f, "({})", self.rule(r).command.to_string_lossy())?;
            }
        }
        Ok(())
    }

    /// Output a .ninja file to do the build
    pub fn write_ninja<F: Write>(&self, f: &mut F) -> io::Result<()> {
        writeln!(f, "commandline = echo replace me")?;
        writeln!(f, "rule sh")?;
        writeln!(f, "  command = $commandline")?;
        writeln!(f)?;

        let mut rules: Vec<_> = self.statuses[Status::Marked].iter().map(|&r| r).collect();
        rules.sort_by_key(|&r| self.pretty_rule(r));

        for r in rules {
            let mut inps: Vec<_> = self.rule(r).all_inputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
                .map(|i| self.pretty_path(i))
                .collect();
            let mut outs: Vec<_> = self.rule(r).all_outputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
                .map(|i| self.pretty_path(i))
                .collect();
            inps.sort();
            outs.sort();
            write!(f, "build ")?;
            for o in outs {
                write!(f, "{} ", o.display())?;
            }
            write!(f, ": sh")?;
            for i in inps {
                write!(f, " {}", i.display())?;
            }
            if self.rule(r).working_directory == self.flags.root {
                writeln!(f, "\n  commandline = {}", self.rule(r).command.to_string_lossy())?;
            } else {
                writeln!(f, "\n  commandline = cd {:?} && {}",
                         self.rule(r).working_directory
                              .strip_prefix(&self.flags.root).unwrap(),
                         self.rule(r).command.to_string_lossy())?;
            }
        }
        Ok(())
    }

    /// Output a tupfile to do the build
    pub fn write_tupfile<F: Write>(&self, f: &mut F) -> io::Result<()> {
        for &r in self.marked_rules.iter() {
            let mut inps: Vec<_> = self.rule(r).all_inputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
                .map(|i| self.pretty_path(i))
                .collect();
            let mut outs: Vec<_> = self.rule(r).all_outputs.iter()
                .map(|&i| i)
                .filter(|&i| self[i].path.starts_with(&self.flags.root))
                .map(|i| self.pretty_path(i))
                .collect();
            inps.sort();
            outs.sort();
            write!(f, ": ")?;
            for i in inps {
                write!(f, "{} ", i.display())?;
            }
            write!(f, "|> ")?;
            if self.rule(r).working_directory == self.flags.root {
                write!(f, "{}", self.rule(r).command.to_string_lossy())?;
            } else {
                write!(f, "cd {:?} && {}",
                       self.rule(r).working_directory
                       .strip_prefix(&self.flags.root).unwrap(),
                       self.rule(r).command.to_string_lossy())?;
            }
            write!(f, " |>")?;
            for o in outs {
                write!(f, " {}", o.display())?;
            }
            writeln!(f)?;
        }
        Ok(())
    }

    /// Add a new File as an input to this rule.
    pub fn add_input(&mut self, r: RuleRef, input: FileRef) {
        // It is a bug to call this on an input that is listed as an
        // output.  This bug would lead to an apparent dependency
        // cycle.  We crash rather than simply removing it from the
        // other set, because we can't tell in this function which set
        // it *should* be in.  Plus better to fix the bug than to hide
        // it with a runtime check.
        assert!(!self.rule(r).all_outputs.contains(&input));
        self.rule_mut(r).all_inputs.insert(input);
        self[input].children.insert(r);
    }
    /// Add a new File as an output of this rule.
    pub fn add_output(&mut self, r: RuleRef, output: FileRef) {
        // It is a bug to call this on an output that is listed as an
        // input.  This bug would lead to an apparent dependency
        // cycle.  We crash rather than simply removing it from the
        // other set, because we can't tell in this function which set
        // it *should* be in.  Plus better to fix the bug than to hide
        // it with a runtime check.
        assert!(!self.rule(r).all_inputs.contains(&output));
        self.rule_mut(r).all_outputs.insert(output);
        self[output].rule = Some(r);
    }

    /// Add a new File as an explicit input to this rule.
    fn add_explicit_input(&mut self, r: RuleRef, input: FileRef) {
        self.rule_mut(r).inputs.push(input);
        self.add_input(r, input);
    }
    /// Add a new File as an explicit output of this rule.
    fn add_explicit_output(&mut self, r: RuleRef, output: FileRef) {
        self.rule_mut(r).outputs.push(output);
        self.add_output(r, output);
    }

    /// Adjust the status of this rule, making sure to keep our sets
    /// up to date.
    pub fn set_status(&mut self, r: RuleRef, s: Status) {
        let oldstatus = self.rule(r).status;
        self.statuses[oldstatus].remove(&r);
        self.statuses[s].insert(r);
        self.rule_mut(r).status = s;
    }

    fn mark(&mut self, r: RuleRef) {
        if self.rule(r).status == Status::Unknown {
            self.definitely_mark(r);
        }
    }

    fn definitely_mark(&mut self, r: RuleRef) {
        if self.rule(r).status != Status::Marked {
            self.set_status(r, Status::Marked);
            let mut to_mark = vec![r];
            while to_mark.len() > 0 {
                let oldlen = to_mark.len();
                let r = *to_mark.last().unwrap();
                let input_rules: Vec<_> = self.rule(r).all_inputs.iter()
                    .flat_map(|&i| self[i].rule)
                    .filter(|&rr| self.rule(rr).status == Status::Unknown)
                    .collect();
                for rr in input_rules {
                    if self.rule(rr).status == Status::Unknown {
                        to_mark.push(rr);
                        self.set_status(rr, Status::Marked);
                    }
                }
                if to_mark.len() == oldlen {
                    to_mark.pop();
                    self.marked_rules.push(r);
                }
            }
        }
    }

    fn mark_all(&mut self) {
        let to_mark: Vec<_> = if self.flags.targets.len() > 0 {
            let mut m = Vec::new();
            let targs = self.flags.targets.clone();
            for p in targs {
                let f = self.new_file(&p);
                if let Some(r) = self[f].rule {
                    m.push(r);
                } else {
                    println!("error: no rule to make target {:?}", &p);
                    self.unlock_repository_and_exit(1);
                }
            }
            m
        } else {
            self.statuses[Status::Unknown].iter()
                .map(|&r| r).filter(|&r| self.rule(r).is_default).collect()
        };
        for r in to_mark {
            self.mark(r);
        }
    }

    /// Is this file built yet?
    pub fn is_file_done(&self, f: FileRef) -> bool {
        if let Some(r) = self[f].rule {
            self.rule(r).status.is_done()
        } else {
            // No rule to build it, so it is inherently done!
            true
        }
    }

    /// For debugging show all rules and their current status.
    pub fn show_rules_status(&self) {
        for r in self.rulerefs() {
            println!(">>> {} is {:?}", self.pretty_rule(r), self.rule(r).status);
        }
    }

    fn check_cleanliness(&mut self, r: RuleRef) {
        vvvprintln!("check_cleanliness {} (currently {:?})",
                    self.pretty_rule(r), self.rule(r).status);
        let old_status = self.rule(r).status;
        if old_status != Status::Unknown && old_status != Status::Unready &&
            old_status != Status::Marked {
                vvprintln!("     Already {:?}: {}", old_status, self.pretty_rule(r));
                return; // We already know if it is clean!
            }
        if self.rule(r).all_inputs.len() == 0 && self.rule(r).all_outputs.len() == 0 {
            // Presumably this means we have never built this rule, and its
            // inputs are in git.
            vvprintln!("     Never been built: {}", self.pretty_rule(r));
            self.set_status(r, Status::Dirty); // FIXME should sort by latency...
        }
        vvprintln!(" ??? Considering cleanliness of {}", self.pretty_rule(r));
        let mut rebuild_excuse: Option<String> = None;
        self.set_status(r, Status::BeingDetermined);
        let mut am_now_unready = false;
        let r_inputs: Vec<FileRef> = self.rule(r).inputs.iter().map(|&i| i).collect();
        let r_all_inputs: HashSet<FileRef> =
            self.rule(r).all_inputs.iter().map(|&i| i).collect();
        let r_other_inputs: HashSet<FileRef> = r_inputs.iter().map(|&i| i).collect();
        let r_other_inputs = &r_all_inputs - &r_other_inputs;
        for &i in r_all_inputs.iter() {
            if let Some(irule) = self[i].rule {
                match self.rule(irule).status {
                    Status::Unknown | Status::Marked => {
                        self.check_cleanliness(irule);
                    },
                    _ => (),
                };
                match self.rule(irule).status {
                    Status::Unknown | Status::Marked => {
                        panic!("This should not happen!?");
                    },
                    Status::BeingDetermined => {
                        // FIXME: nicer error handling would be great here.
                        println!("error: cycle involving:\n\t{}\n\t{:?}\n\t{}",
                                 self.pretty_rule(r), self.pretty_path(i),
                                 self.pretty_rule(irule));
                        self.unlock_repository_and_exit(1);
                    },
                    Status::Dirty | Status::Unready | Status::Building => {
                        am_now_unready = true;
                    },
                    Status::Clean | Status::Failed | Status::Built => {
                        // Nothing to do here
                    },
                }
            }
        }
        self.set_status(r, old_status);
        if am_now_unready {
            vvprintln!(" !!! Unready for {}", self.pretty_rule(r));
            self.set_status(r, Status::Unready);
            return;
        }
        let mut is_dirty = false;

        for &i in r_inputs.iter() {
            if !self[i].in_git() &&
                self[i].rule.is_none() &&
                self[i].path.starts_with(&self.flags.root) &&
                !self.is_git_path(&self[i].path) {
                    // One of our explicit inputs is not in git, and
                    // we also do not know how to build it yet.  One
                    // hopes that there is some rule that will produce
                    // it!  :)
                    vvprintln!(" !!! Explicit input {:?} (i.e. {:?} not in git for {}",
                              self.pretty_path_peek(i), self[i].path,
                              self.pretty_rule(r));
                    self.set_status(r, Status::Unready);
                    return;
                }
        }
        for &i in r_other_inputs.iter() {
            self[i].stat().ok(); // check if it is a directory!
            if !self[i].in_git() &&
                self[i].rule.is_none() &&
                self[i].path.starts_with(&self.flags.root) &&
                !self.is_git_path(&self[i].path) &&
                self[i].hashstat.kind != Some(FileKind::Dir) {
                    // One of our implicit inputs is not in git, and
                    // we also do not know how to build it.  But it
                    // was previously present and observed as an
                    // input.  This should be rebuilt since it may
                    // have "depended" on that input via something
                    // like "cat *.input > foo", and now that it
                    // doesn't exist we must rebuild.
                    rebuild_excuse = rebuild_excuse.or(
                        Some(format!("input {:?} has no rule",
                                     self.pretty_path_peek(i))));
                    is_dirty = true;
                }
        }
        if !is_dirty {
            for &i in r_all_inputs.iter() {
                let path = self[i].path.clone();
                self[i].hashstat.stat(&path).ok();
                if let Some(istat) = self.rule(r).hashstats.get(&i).map(|s| *s) {
                    if let Some(irule) = self[i].rule {
                        if self.rule(irule).status == Status::Built {
                            if self[i].hashstat.cheap_matches(&istat) {
                                // nothing to do here
                            } else if self[i].hashstat.matches(&path, &istat) {
                                // the following handles updating the
                                // file stats in the rule, so next
                                // time a cheap match will be enough.
                                let newstat = self[i].hashstat;
                                self.rule_mut(r).hashstats.insert(i, newstat);
                                let facfile = self.rule(r).facfile;
                                self.facfiles_used.insert(facfile);
                            } else {
                                rebuild_excuse = rebuild_excuse.or(
                                    Some(format!("{:?} has been rebuilt",
                                                 self.pretty_path_peek(i))));
                                is_dirty = true;
                                break;
                            }
                            continue; // this input is fine
                        }
                    }
                    // We did not just build it, so different excuses!
                    if self[i].hashstat.cheap_matches(&istat) {
                        // nothing to do here
                    } else if self.rule(r).is_cache(&path) {
                        // it is now treated as cache, so ignore it!
                    } else if self[i].hashstat.matches(&path, &istat) {
                        // the following handles updating the file
                        // stats in the rule, so next time a cheap
                        // match will be enough.
                        let newstat = self[i].hashstat;
                        self.rule_mut(r).hashstats.insert(i, newstat);
                        let facfile = self.rule(r).facfile;
                        self.facfiles_used.insert(facfile);
                    } else if !self[i].hashstat.env_matches(&istat) {
                        rebuild_excuse = rebuild_excuse.or(
                            Some(format!("the environment has changed")));
                        is_dirty = true;
                        break;
                    } else {
                        rebuild_excuse = rebuild_excuse.or(
                            Some(format!("{:?} has been modified",
                                         self.pretty_path_peek(i))));
                        is_dirty = true;
                        break;
                    }

                } else if self[i].hashstat.kind != Some(FileKind::Dir) {
                    // In case of an input that is a directory, if it
                    // has no input time, we conclude that it wasn't
                    // actually readdired, and only needs to exist.
                    // Otherwise, if there is no input time, something
                    // is weird and we must need to rebuild.
                    rebuild_excuse = rebuild_excuse.or(
                        Some(format!("have no information on {:?}",
                                     self.pretty_path_peek(i))));
                    is_dirty = true;
                    break;
                }
            }
        }
        if !is_dirty {
            let r_all_outputs: HashSet<FileRef> =
                self.rule(r).all_outputs.iter().map(|&o| o).collect();
            if r_all_outputs.len() == 0 {
                rebuild_excuse = rebuild_excuse.or(
                    Some(format!("it has never been run")));
                is_dirty = true;
            }
            for o in r_all_outputs {
                if let Some(ostat) = self.rule(r).hashstats.get(&o).map(|s| *s) {
                    let path = self[o].path.clone();
                    if !self[o].exists() {
                        rebuild_excuse = rebuild_excuse.or(
                            Some(format!("output {:?} does not exist",
                                         self.pretty_path_peek(o))));
                        is_dirty = true;
                        break;
                    } else if self[o].hashstat.cheap_matches(&ostat) {
                        // nothing to do here
                    } else if self.rule(r).is_cache(&path) {
                        // it is now treated as cache, so ignore it!
                    } else if self[o].hashstat.matches(&path, &ostat) {
                        // the following handles updating the file
                        // stats in the rule, so next time a cheap
                        // match will be enough.
                        let newstat = self[o].hashstat;
                        self.rule_mut(r).hashstats.insert(o, newstat);
                        let facfile = self.rule(r).facfile;
                        self.facfiles_used.insert(facfile);
                    } else if !self[o].hashstat.env_matches(&ostat) {
                        rebuild_excuse = rebuild_excuse.or(
                            Some(format!("the environment has changed")));
                        is_dirty = true;
                        break;
                    } else if self[o].hashstat.kind == Some(FileKind::Dir) {
                        // If the rule creates a directory, we want to
                        // ignore any changes within that directory,
                        // there is no reason to rebuild just because
                        // the directory contents changed.
                    } else {
                        rebuild_excuse = rebuild_excuse.or(
                            Some(format!("output {:?} has been modified",
                                         self.pretty_path_peek(o))));
                        is_dirty = true;
                        break;
                    }
                } else {
                    rebuild_excuse = rebuild_excuse.or(
                        Some(format!("have never built {:?}",
                                     self.pretty_path_peek(o))));
                    is_dirty = true;
                    break;
                }
            }
        }
        if is_dirty {
            vprintln!(" *** Building {}\n     because {}",
                      self.pretty_rule(r),
                      rebuild_excuse.unwrap_or(String::from("I am confused?")));
            self.dirty(r);
        } else {
            vvprintln!(" *** Clean: {}", self.pretty_rule(r));
            self.set_status(r, Status::Clean);
            self.read_facfiles_from_rule(r);
        }
    }

    /// Mark this rule as dirty, adjusting other rules to match.
    fn dirty(&mut self, r: RuleRef) {
        let oldstat = self.rule(r).status;
        if oldstat != Status::Dirty {
            self.set_status(r, Status::Dirty);
            if oldstat != Status::Unready {
                // Need to inform marked child rules they are unready now
                let children: Vec<RuleRef> = self.rule(r).outputs.iter()
                    .flat_map(|o| self[*o].children.iter()).map(|c| *c)
                    .filter(|&c| self.rule(c).status == Status::Marked).collect();
                // This is a separate loop to satisfy the borrow checker.
                for childr in children.iter() {
                    self.unready(*childr);
                }
            }
        }
    }
    /// Make this rule (and any that depend on it) `Status::Unready`.
    fn unready(&mut self, r: RuleRef) {
        if self.rule(r).status != Status::Unready {
            let mut children = vec![r];

            while children.len() > 0 {
                let mut grandchildren = Vec::new();
                for r in children {
                    vvvprintln!("child unready (was {:?}):  {}",
                                self.rule(r).status, self.pretty_rule(r));
                    self.set_status(r, Status::Unready);
                    // Need to inform marked child rules they are unready now
                    grandchildren.extend(self.rule(r).outputs.iter()
                                         .flat_map(|o| self[*o].children.iter()).map(|c| *c)
                                         .filter(|&c| self.rule(c).status != Status::Unready
                                                 && self.rule(c).status != Status::Unknown));
                }
                children = grandchildren;
            }
        }
    }
    fn failed(&mut self, r: RuleRef) {
        if self.rule(r).status != Status::Failed {
            let mut children = vec![r];

            while children.len() > 0 {
                let mut grandchildren = Vec::new();
                for r in children {
                    self.set_status(r, Status::Failed);
                    // Need to inform marked child rules they are unready now
                    grandchildren.extend(self.rule(r).outputs.iter()
                                         .flat_map(|o| self[*o].children.iter()).map(|c| *c)
                                         .filter(|&c| self.rule(c).status == Status::Unready));
                }
                children = grandchildren;
            }
            // Delete any files that were created, so that they will
            // be properly re-created next time this command is run.
            for &o in self.rule(r).all_outputs.iter() {
                if !self[o].is_in_git {
                    self[o].unlink();
                }
            }
        }
    }

    fn read_facfiles_from_rule(&mut self, r: RuleRef) {
        let outputs: Vec<_> = self.rule(r).outputs.iter().map(|&o| o)
            .filter(|&o| self[o].is_fac_file()).collect();
        for o in outputs {
            if let Some(_) = self[o].rules_defined {
                // We have read this facfile before, so we need to
                // be cautious, because rules may have changed.
                unimplemented!()
            }
            if let Err(e) = self.read_facfile(o) {
                println!("{}", e);
                self.unlock_repository_and_exit(1);
            }
        }
    }

    fn built(&mut self, r: RuleRef) {
        self.set_status(r, Status::Built);
        // If we built a facfile, we should then read it!
        self.read_facfiles_from_rule(r);
        // Mark for checking the children of this rule, as they may
        // now be buildable.
        let outputs: Vec<_> = self.rule(r).all_outputs.iter().map(|&o| o).collect();
        for o in outputs {
            self.modified_file(o);
        }
    }

    /// We have modified this file.  Either the user changed it, or we
    /// have built it.  Either way, we need to respond by marking its
    /// children as possibly ready.
    fn modified_file(&mut self, f: FileRef) {
        let children: Vec<RuleRef> = self[f].children.iter()
            .map(|&c| c)
            .filter(|&c| self.rule(c).status == Status::Unready
                    || self.rule(c).status == Status::Failed
                    || self.rule(c).status == Status::Clean
                    || self.rule(c).status == Status::Built).collect();
        for c in children {
            // Next time examine the children to see if we can build them.
            self.definitely_mark(c);
        }
    }

    fn summarize_build_results(&self) -> i32 {
        if self.statuses[Status::Failed].len() > 0 {
            println!("Build failed {}/{} failures",
                     self.statuses[Status::Failed].len(),
                     self.statuses[Status::Failed].len()
                     + self.statuses[Status::Built].len());
            if self.flags.dry_run {
                println!("But it is only a dry run, so it's all cool!");
                return 0;
            }
            return self.statuses[Status::Failed].len() as i32;
        }
        if let Some(err) = self.check_strictness() {
            println!("Build failed due to {}!", err);
            1
        } else {
            println!("Build succeeded!");
            0
        }
    }

    fn check_strictness(&self) -> Option<String> {
        match self.flags.strictness {
            flags::Strictness::Strict => {
                // Checking for strict dependencies is harder than for
                // exhaustive, because we need to ensure that
                // everything will be built prior to being needed.
                // For each input there must be an explicit input that
                // has the same rule that generates that input.
                println!("Checking for strict dependencies...");
                let mut fails = None;
                for r in self.rulerefs() {
                    for &i in self.rule(r).all_inputs.iter() {
                        if !self.rule(r).inputs.contains(&i) {
                            if self[i].rule.is_some() {
                                let mut have_rule = false;
                                for &j in self.rule(r).inputs.iter() {
                                    have_rule = have_rule || (self[j].rule == self[i].rule)
                                }
                                if !have_rule {
                                    println!("missing dependency: \"{}\" requires {}",
                                             self.pretty_rule(r),
                                             self.pretty_path_peek(i).display());
                                    fails = Some(String::from("missing dependencies"));
                                }
                            }
                        }
                    }
                }
                fails
            },
            flags::Strictness::Exhaustive => {
                // Here we just need to check that there are NO local
                // inputs (i.e. ones in our repository directory) for
                // any rules that are not explicit.
                println!("Checking for exhaustive dependencies...");
                let mut fails = None;
                for r in self.rulerefs() {
                    for i in self.rule(r).all_inputs.iter()
                        .map(|&i| i)
                        .filter(|&i| self[i].path.starts_with(&self.flags.root))
                        .filter(|&i| !self.rule(r).inputs.contains(&i))
                    {
                        println!("missing dependency: \"{}\" requires {}",
                                 self.pretty_rule(r),
                                 self.pretty_path_peek(i).display());
                        fails = Some(String::from("missing dependencies"));
                    }
                    for i in self.rule(r).all_outputs.iter()
                        .map(|&i| i)
                        .filter(|&i| !self.rule(r).outputs.contains(&i))
                        .filter(|&i| self[i].children.len() > 0)
                    {
                        println!("missing output: \"{}\" builds {}",
                                 self.pretty_rule(r),
                                 self.pretty_path_peek(i).display());
                        fails = Some(String::from("missing outputs"));
                    }
                }
                fails
            },
            flags::Strictness::Normal => {
                None
            },
        }
    }

    fn num_building(&self) -> usize {
        if !self.flags.dry_run {
            assert_eq!(self.process_killers.len(), self.statuses[Status::Building].len());
        }
        self.statuses[Status::Building].len()
    }
    /// Actually run a command.  This needs to update the inputs and
    /// outputs.  FIXME
    pub fn spawn(&mut self, r: RuleRef) -> io::Result<()> {
        {
            // Before running, let us fill in any hash info for
            // inputs that we have not yet read about.
            let v: Vec<FileRef> = self.rule(r).all_inputs.iter()
                .filter(|&w| self[*w].hashstat.unfinished()).map(|&w|w).collect();
            for w in v {
                let p = self[w].path.clone(); // ugly workaround for borrow checker
                self[w].hashstat.finish(&p).unwrap();
            }
        }
        let srs = self.send_rule_status.clone();
        if self.flags.dry_run {
            // We do not actually want to run anything! Just treat the
            // command as failed.
            std::thread::spawn(move || {
                let e = Err(io::Error::new(io::ErrorKind::Other, "dry run"));
                srs.send(Event::Finished(r, e)).ok();
            });
            self.set_status(r, Status::Building);
            return Ok(());
        }
        let wd = self.flags.root.join(&self.rule(r).working_directory);
        let mut cmd = bigbro::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&self.rule(r).command)
            .blind(self.flags.blind)
            .current_dir(&wd)
            .stdin(bigbro::Stdio::null());
        if let Some(ref logdir) = self.flags.log_output {
            std::fs::create_dir_all(logdir)?;
            let d = self.flags.root.join(logdir).join(self.sanitize_rule(r));
            cmd.log_stdouterr(&d);
        } else {
            cmd.save_stdouterr();
        }
        let kill_child = cmd.spawn_and_hook(move |s| {
            srs.send(Event::Finished(r, s)).ok();
        })?;
        self.set_status(r, Status::Building);
        self.process_killers.insert(r, kill_child);
        Ok(())
    }
    fn wait_for_a_rule(&mut self) {
        match self.recv_rule_status.recv() {
            Ok(Event::Finished(rr,Ok(stat))) =>
                if let Err(e) = self.finish_rule(rr, stat) {
                    println!("error finishing rule? {}", e);
                },
            Ok(Event::Finished(rr,Err(e))) => {
                let num_built = 1 + self.statuses[Status::Failed].len()
                    + self.statuses[Status::Built].len();
                let num_total = self.statuses[Status::Failed].len()
                    + self.statuses[Status::Built].len()
                    + self.statuses[Status::Building].len()
                    + self.statuses[Status::Dirty].len()
                    + self.statuses[Status::Unready].len();
                println!("!{}/{}! {}: {}", num_built, num_total, e, self.pretty_rule(rr));
                self.failed(rr);
                self.process_killers.remove(&rr);
            },
            Ok(Event::NotifyChange(_)) => {
                println!("FIXME: File changed already?!");
            },
            Err(e) => {
                println!("error receiving status: {}", e);
                self.am_interrupted = true;
                self.unlock_repository_and_exit(1);
            },
            Ok(Event::CtrlC) => {
                if !self.am_interrupted {
                    self.am_interrupted = true;
                    println!("Interrupted!");
                    self.unlock_repository_and_exit(1);
                }
            },
        };
    }
    fn check_for_a_rule(&mut self) -> bool {
        match self.recv_rule_status.try_recv() {
            Ok(Event::Finished(rr,Ok(stat))) => {
                if let Err(e) = self.finish_rule(rr, stat) {
                    println!("error finishing rule? {}", e);
                }
                true
            },
            Ok(Event::Finished(rr,Err(e))) => {
                println!("error running rule: {} {}", self.pretty_rule(rr), e);
                self.process_killers.remove(&rr);
                true
            },
            Ok(Event::NotifyChange(_)) => {
                println!("FIXME: File changed already?!");
                false
            },
            Err(e) => {
                if !self.am_interrupted {
                    println!("error receiving status: {}", e);
                    self.am_interrupted = true;
                    self.unlock_repository_and_exit(1);
                }
                false
            },
            Ok(Event::CtrlC) => {
                if !self.am_interrupted {
                    self.am_interrupted = true;
                    println!("Interrupted!");
                    self.unlock_repository_and_exit(1);
                }
                true
            },
        }
    }

    /// Remove the output of a failed rule
    fn clean_output(&self, stat: &bigbro::Status) {
        for w in stat.written_to_files() {
            if self.filemap.get(&w).map(|&f| self[f].is_in_git) != Some(true) {
                std::fs::remove_file(w).ok(); // output is not in git, so we can delete
            }
        }
        let mut dirs: Vec<_> = stat.mkdir_directories().iter().map(|d| d.clone()).collect();
        dirs.sort_by_key(|d| - (d.to_string_lossy().len() as i32));
        for d in dirs {
            std::fs::remove_dir(&d).ok();
        }
    }

    /// Handle a rule finishing.
    pub fn finish_rule(&mut self, r: RuleRef, mut stat: bigbro::Status) -> io::Result<()> {
        self.process_killers.remove(&r);
        let num_built = 1 + self.statuses[Status::Failed].len()
            + self.statuses[Status::Built].len();
        let num_total = self.statuses[Status::Failed].len()
            + self.statuses[Status::Built].len()
            + self.statuses[Status::Building].len()
            + self.statuses[Status::Dirty].len()
            + self.statuses[Status::Unready].len();
        let message: String;
        let abort = |sel: &mut Build, stat: &bigbro::Status, errmsg: &str| -> io::Result<()> {
            println!("error: {}", errmsg);
            sel.failed(r);
            sel.clean_output(stat);
            let ff = sel.rule(r).facfile;
            sel.facfiles_used.insert(ff);
            Ok(())
        };

        if stat.status().success() {
            let mut written_to_files = stat.written_to_files();
            let mut read_from_files = stat.read_from_files();
            let mut rule_actually_failed = false;
            // First clear out the listing of inputs and outputs.
            let old_inputs: Vec<_> = self.rule_mut(r).all_inputs.drain().collect();
            for i in old_inputs {
                // Make sure to clear out "children" relationship when
                // removing inputs.  We'll add back the proper links
                // later.
                self[i].children.remove(&r);
            }
            let mut old_outputs: HashSet<FileRef> =
                self.rule_mut(r).all_outputs.drain().collect();
            // First we add in our explicit inputs.  This is done
            // first, because we want to ensure that we don't count an
            // explicit input as an output.
            let explicit_inputs = self.rule(r).inputs.clone();
            for i in explicit_inputs {
                self.add_input(r, i);
                let hs = self[i].hashstat;
                if hs.kind != Some(FileKind::Dir) {
                    // for files and symlinks that were not read, we
                    // want to know what their hash was, so we won't
                    // rebuild on their behalf.
                    self.rule_mut(r).hashstats.insert(i, hs);
                }
                written_to_files.remove(&self[i].path);
                read_from_files.remove(&self[i].path);
            }
            for w in written_to_files {
                if w.starts_with(&self.flags.root)
                    && !self.is_git_path(&w)
                    && !self.rule(r).is_cache(&w) {
                        let fw = self.new_file(&w);
                        if self[fw].hashstat.finish(&w).is_ok() {
                            if let Some(fwr) = self[fw].rule {
                                if fwr != r {
                                    let mess = format!("two rules generate same output {:?}:\n\t{}\nand\n\t{}",
                                                       self.pretty_path_peek(fw),
                                                       self.pretty_rule(r),
                                                       self.pretty_rule(fwr));
                                    return abort(self, &stat, &mess);
                                }
                            }
                            self.add_output(r, fw);
                            // update the hashstat for the output that changed
                            let hs = self[fw].hashstat;
                            self.rule_mut(r).hashstats.insert(fw, hs);
                            old_outputs.remove(&fw);
                        } else {
                            vprintln!("   Hash not okay?!");
                        }
                    }
            }
            for d in stat.mkdir_directories() {
                if d.starts_with(&self.flags.root)
                    && !self.is_git_path(&d)
                    && !self.rule(r).is_cache(&d)
                {
                    let fw = self.new_file(&d);
                    if self[fw].hashstat.finish(&d).is_ok() {
                        // We allow multiple rules to mkdir the same
                        // directory.  This is fine, since we do not
                        // apply strict ordering to the creation of a
                        // directory.
                        self.add_output(r, fw);
                        old_outputs.remove(&fw);
                    }
                }
            }
            for rr in read_from_files {
                if !self.is_boring(&rr) && !self.rule(r).is_cache(&rr) {
                    let fr = self.new_file(&rr);
                    if !old_outputs.contains(&fr)
                        && self[fr].hashstat.finish(&rr).is_ok()
                    {
                        let hs = self[fr].hashstat;
                        self.rule_mut(r).hashstats.insert(fr, hs);
                        if rr.starts_with(&self.flags.root)
                            && !self[fr].rule.is_some()
                            && !self[fr].is_in_git
                            && !self.is_git_path(&rr)
                        {
                            if self.flags.git_add {
                                if let Err(e) = git::add(&self[fr].path) {
                                    rule_actually_failed = true;
                                    println!("error: unable to git add {:?} successfully:",
                                             self.pretty_path_peek(fr));
                                    println!("{}", e);
                                } else {
                                    self[fr].is_in_git = true;
                                }
                            } else {
                                rule_actually_failed = true;
                                println!("error: {:?} should be in git for {}",
                                         self.pretty_path_peek(fr), self.pretty_reason(r));
                            }
                        }
                        self.add_input(r, fr);
                    }
                }
            }
            for rr in stat.read_from_directories() {
                if !self.is_boring(&rr) && !self.rule(r).is_cache(&rr) {
                    let fr = self.new_file(&rr);
                    if self[fr].hashstat.finish(&rr).is_ok() && !old_outputs.contains(&fr) {
                        let hs = self[fr].hashstat;
                        self.rule_mut(r).hashstats.insert(fr, hs);
                        self.add_input(r, fr);
                    }
                }
            }
            // Here we add in any explicit outputs that were not
            // actually touched.
            let explicit_outputs = self.rule(r).outputs.clone();
            for o in explicit_outputs {
                if !self.rule(r).all_outputs.contains(&o) {
                    self.add_output(r, o);
                    if !self[o].exists() {
                        println!("build failed to create: {:?}",
                                 self.pretty_path_peek(o));
                        rule_actually_failed = true;
                    }
                }
            }
            for o in old_outputs {
                // Any previously created files that still exist
                // should be treated as if they were created this time
                // around.
                if self[o].exists() && !self.rule(r).is_cache(&self[o].path) {
                    self.add_output(r, o);
                }
            }
            {
                // let us fill in any hash info for outputs that were
                // not touched in this build.
                let v: Vec<FileRef> = self.rule(r).all_outputs.iter()
                    .filter(|&w| self[*w].hashstat.unfinished()).map(|&w|w).collect();
                for w in v {
                    let p = self[w].path.clone();
                    if self[w].hashstat.finish(&p).is_err() {
                        // presumably this file no longer exists, so
                        // we should remove it from the list of
                        // outputs.
                        self.rule_mut(r).all_outputs.remove(&w);
                    }
                }
            }
            if rule_actually_failed {
                message = format!("build failed: {}", self.pretty_rule(r));
                println!("!{}/{}!: {}",
                         num_built, num_total, message);
                self.failed(r);
                self.clean_output(&stat);
            } else {
                message = self.pretty_rule(r);
                println!("[{}/{}]: {}", num_built, num_total, &message);
                self.built(r);
            }
        } else {
            message = format!("build failed: {}", self.pretty_rule(r));
            println!("!{}/{}!: {}",
                     num_built, num_total, message);
            self.failed(r);
            self.clean_output(&stat);
        }
        if self.flags.show_output || !stat.status().success() {
            let f = stat.stdout()?;
            let mut contents = String::new();
            f.unwrap().read_to_string(&mut contents)?;
            if contents.len() > 0 {
                println!("{}", contents);
                println!("end of output from: {}", &message);
            }
        }
        let ff = self.rule(r).facfile;
        self.facfiles_used.insert(ff);
        Ok(())
    }

    /// Formats the path nicely as a relative path if possible
    pub fn pretty_path(&self, p: FileRef) -> PathBuf {
        if self[p].path == self.flags.root {
            return PathBuf::from(".")
        }
        match self[p].path.strip_prefix(&self.flags.root) {
            Ok(p) => PathBuf::from(p),
            Err(_) => self[p].path.clone(),
        }
    }
    /// Formats the path nicely as a relative path if possible
    pub fn pretty_path_peek(&self, p: FileRef) -> &Path {
        if self[p].path == self.flags.root {
            return &Path::new(".")
        }
        match self[p].path.strip_prefix(&self.flags.root) {
            Ok(p) => p,
            Err(_) => &self[p].path,
        }
    }

    fn is_local(&self, p: FileRef) -> bool {
        self[p].path.starts_with(&self.flags.root)
    }

    /// Formats the rule nicely if possible
    pub fn pretty_rule(&self, r: RuleRef) -> String {
        if self.rule(r).working_directory == self.flags.root {
            self.rule(r).command.to_string_lossy().into_owned()
        } else {
            format!("{}: {}",
                    self.rule(r).working_directory
                        .strip_prefix(&self.flags.root).unwrap().display(),
                    self.rule(r).command.to_string_lossy())
        }
    }

    /// pretty_reason is a way of describing a rule in terms of why it
    /// needs to be built.  If the rule is always built by default, it
    /// gives the same output that pretty_rule does.  However, if it
    /// is a non-default rule, it selects an output which is actually
    /// needed to describe why it needs to be built.
    pub fn pretty_reason(&self, r: RuleRef) -> String {
        if self.rule(r).is_default {
            return self.pretty_rule(r);
        }
        for &o in self.rule(r).outputs.iter() {
            for &c in self[o].children.iter() {
                if self.rule(c).status == Status::Unready
                    || self.rule(c).status == Status::Failed
                {
                    return format!("{}", self.pretty_path_peek(o).display());
                }
            }
        }
        self.pretty_rule(r) // ?!
    }

    /// pretty_rule_output gives a filepath that would be built by the
    /// rule.
    pub fn pretty_rule_output(&self, r: RuleRef) -> PathBuf {
        if self.rule(r).outputs.len() > 0 {
            self.pretty_path(self.rule(r).outputs[0])
        } else {
            let mut paths: Vec<_> = self.rule(r).all_outputs.iter()
                .map(|&o| self.pretty_path(o)).collect();
            paths.sort();
            paths.sort_by_key(|p| p.to_string_lossy().len());
            paths[0].clone()
        }
    }


    /// Formats the rule as a sane filename
    fn sanitize_rule(&self, r: RuleRef) -> PathBuf {
        let rr = if self.rule(r).outputs.len() == 1 {
            format!("{}", self.pretty_path_peek(self.rule(r).outputs[0]).display())
        } else {
            self.pretty_rule(r)
        };
        let mut buf = String::with_capacity(rr.len());
        for c in rr.chars() {
            match c {
                'a' ... 'z' | 'A' ... 'Z' | '0' ... '9' | '_' | '-' | '.' => buf.push(c),
                _ => buf.push('_'),
            }
        }
        PathBuf::from(buf)
    }

    /// Look up the rule
    pub fn rule_mut(&mut self, r: RuleRef) -> &mut Rule {
        &mut self.rules[r.0]
    }
    /// Look up the rule
    pub fn rule(&self, r: RuleRef) -> &Rule {
        &self.rules[r.0]
    }


    /// This is a path in the git repository that we should ignore
    pub fn is_git_path(&self, path: &Path) -> bool {
        if let Ok(path) = path.strip_prefix(&self.flags.root) {
            return path.starts_with(".git") && !path.starts_with(".git/hooks")
        }
        false
    }

    /// This path is inherently boring
    pub fn is_boring(&self, path: &Path) -> bool {
        path.starts_with("/proc") || path.starts_with("/dev") ||
            // the following is intended for /etc/ld.so.cache, but I
            // think any path outside of your repository that ends
            // with ".cache" can be assumed to be a cache.
            (path.is_absolute() && path.extension() == Some(OsStr::new("cache"))) ||
            // the following is intended for ~/.cache/, but presumably
            // any directory named ".cache" can be assumed to be a
            // cache.
            (path.is_absolute()
             && path.components().any(|c| c.as_ref() == OsStr::new(".cache")))
    }
}

impl std::ops::Index<FileRef> for Build  {
    type Output = File;
    fn index(&self, r: FileRef) -> &File {
        &self.files[r.0]
    }
}
impl std::ops::IndexMut<FileRef> for Build  {
    fn index_mut(&mut self, r: FileRef) -> &mut File {
        &mut self.files[r.0]
    }
}

#[cfg(unix)]
fn bytes_to_osstr(b: &[u8]) -> &OsStr {
    OsStr::from_bytes(b)
}

#[cfg(not(unix))]
fn bytes_to_osstr(b: &[u8]) -> &OsStr {
    Path::new(std::str::from_utf8(b).unwrap()).as_os_str()
}

fn normalize(p: &Path) -> PathBuf {
    // This should give the symlink-resolved path that corresponds to
    // what symlink_metadata would see, i.e. it does not resolve the
    // last componenet of the path.
    if let Some(parent) = p.parent() {
        let mut out = if let Ok(pp) = parent.canonicalize() {
            // The parent exists, so we can just use a single call to
            // canonicalize, which is faster than the below case.
            pp
        } else {
            // The parent doesn't exist, so we need to use our manual
            // version of canonicalize, which canonicalizes what it
            // can, and ignores a case where the path does not exist.
            let mut out = PathBuf::new();
            for element in parent.iter() {
                if element == ".." {
                    out.pop();
                } else {
                    out.push(element);
                }
                // The following call to read_link checks if the last
                // element of out is a symlink.  If that is the case, we
                // need to canonicalize.  This saves some time, since
                // canonicalize is much slower than read_link, as it must
                // examine every element of the path (most of which we
                // have already examined).  Note that this slows down the
                // unusual case in which we have a symlink, in order to
                // speed up the common case.
                if out.read_link().is_ok() {
                    if let Ok(o) = out.canonicalize() {
                        out = o;
                    }
                }
            }
            out
        };
        if let Some(filename) = p.file_name() {
            out.push(filename);
        } else {
            out.pop();
        }
        out
    } else {
        PathBuf::from(p)
    }
}

fn cp_to_dir(x: &Path, dir: &Path) -> std::io::Result<()> {
    assert!(!x.is_absolute());
    let newfile = dir.join(x);
    if let Some(d) = newfile.parent() {
        vprintln!("mkdir -p {:?}", &d);
        std::fs::create_dir_all(d)?;
    }
    vprintln!("cp {:?} {:?}", &x, &newfile);
    std::fs::copy(x, newfile)?;
    Ok(())
}
