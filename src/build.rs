//! Rules and types for building the stuff.

#[cfg(test)]
extern crate quickcheck;

extern crate typed_arena;
extern crate bigbro;

use std;

use std::cell::{Cell, RefCell};
use refset::{RefSet};
use std::ffi::{OsString, OsStr};
use std::path::{Path,PathBuf};

use std::collections::{HashSet, HashMap};

use std::io::{Read};

use git;

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

/// Is the file a regular file, a symlink, or a directory?
#[derive(PartialEq, Eq, Hash, Copy, Clone)]
pub enum FileKind {
    /// It is a regular file
    File,
    /// It is a directory
    Dir,
    /// It is a symlink
    Symlink,
}

/// A file (or directory) that is either an input or an output for
/// some rule.
pub struct File<'a> {
    rule: RefCell<Option<&'a Rule<'a>>>,
    path: PathBuf,
    // Question: could Vec be more efficient than RefSet here? It
    // depends if we add a rule multiple times to the same set of
    // children.  FIXME check this!
    children: RefCell<RefSet<'a, Rule<'a>>>,

    rules_defined: RefCell<RefSet<'a, Rule<'a>>>,

    kind: Cell<Option<FileKind>>,
    is_in_git: bool,
}

impl<'a> File<'a> {
    /// Declare that this File is dirty (i.e. has been modified since
    /// the last build).
    pub fn dirty(&self) {
        for r in self.children.borrow().iter() {
            r.dirty();
        }
    }

    /// Set file properties...
    pub fn stat(&self) -> std::io::Result<FileKind> {
        let attr = std::fs::metadata(&self.path)?;
        self.kind.set(if attr.file_type().is_symlink() {
            Some(FileKind::Symlink)
        } else if attr.file_type().is_dir() {
            Some(FileKind::Dir)
        } else if attr.file_type().is_file() {
            Some(FileKind::File)
        } else {
            None
        });
        match self.kind.get() {
            Some(k) => Ok(k),
            None => Err(std::io::Error::new(std::io::ErrorKind::Other, "irregular file")),
        }
    }

    /// Is this `File` in git?
    pub fn in_git(&self) -> bool {
        self.is_in_git
    }

    /// Is this a fac file?
    pub fn is_fac_file(&self) -> bool {
        self.rules_defined.borrow().len() > 0
    }

    /// Formats the path nicely as a relative path if possible
    pub fn pretty_path(&self, b: &Build) -> PathBuf {
        match self.path.strip_prefix(&b.root) {
            Ok(p) => PathBuf::from(p),
            Err(_) => self.path.clone(),
        }
    }
}

/// A rule for building something.
pub struct Rule<'a> {
    inputs: RefCell<Vec<&'a File<'a>>>,
    outputs: RefCell<Vec<&'a File<'a>>>,

    status: Cell<Status>,
    cache_prefixes: HashSet<OsString>,
    cache_suffixes: HashSet<OsString>,

    working_directory: PathBuf,
    facfile: &'a File<'a>,
    command: OsString,
}

impl<'a> Rule<'a> {
    /// Add a new File as an input to this rule.
    pub fn add_input(&'a self, input: &'a File<'a>) -> &Rule<'a> {
        self.inputs.borrow_mut().push(input);
        input.children.borrow_mut().insert(self);
        self
    }
    /// Add a new File as an output of this rule.
    pub fn add_output(&'a self, input: &'a File<'a>) -> &Rule<'a> {
        self.outputs.borrow_mut().push(input);
        *input.rule.borrow_mut() = Some(self);
        self
    }
    /// Adjust the status of this rule, making sure to keep our sets
    /// up to date.
    pub fn set_status(&self, b: &'a Build, s: Status) {
        b.statuses[self.status.get()].borrow_mut().remove(self);
        b.statuses[s].borrow_mut().insert(self);
        self.status.set(s);
    }
    /// Mark this rule as dirty, adjusting other rules to match.
    pub fn dirty(&'a self) {
        let oldstat = self.status.get();
        if oldstat != Status::Dirty {
            self.set_status(Status::Dirty);
            if oldstat != Status::Unready {
                // Need to inform child rules they are unready now
                for o in self.outputs.borrow().iter() {
                    for r in o.children.borrow().iter() {
                        r.unready();
                    }
                }
            }
        }
    }
    /// Make this rule (and any that depend on it) `Status::Unready`.
    pub fn unready(&'a self) {
        if self.status.get() != Status::Unready {
            self.set_status(Status::Unready);
            // Need to inform child rules they are unready now
            for o in self.outputs.borrow().iter() {
                for r in o.children.borrow().iter() {
                    r.unready();
                }
            }
        }
    }

    /// Identifies whether a given path is "cache"
    pub fn is_cache(&self, path: &Path) -> bool {
        self.cache_suffixes.iter().any(|s| is_suffix(path, s)) ||
            self.cache_prefixes.iter().any(|s| is_prefix(path, s))
    }

    /// Actually run the command FJIXME
    pub fn run(&mut self, b: &Build) {
        bigbro::Command::new("sh").arg("-c").arg(&self.command)
            .current_dir(&self.working_directory).status().unwrap();
        b.facfiles_used.borrow_mut().insert(self.facfile);
    }
}

use std::os::unix::ffi::{OsStrExt};
fn is_suffix(path: &Path, suff: &OsStr) -> bool {
    let l = suff.as_bytes().len();
    path.as_os_str().as_bytes()[..l] == suff.as_bytes()[..]
}
fn is_prefix(path: &Path, suff: &OsStr) -> bool {
    let l = suff.as_bytes().len();
    let p = path.as_os_str().as_bytes();
    p[p.len()-l..] == suff.as_bytes()[..]
}

/// A struct that holds all the information needed to build.  You can
/// think of this as behaving like a set of global variables, but we
/// can drop the whole thing.  It is implmented using arena
/// allocation, so all of our Rules and Files are guaranteed to live
/// as long as the Build lives.
pub struct Build<'a> {
    alloc_files: &'a typed_arena::Arena<File<'a>>,
    alloc_rules: &'a typed_arena::Arena<Rule<'a>>,
    files: RefCell<HashMap<&'a Path, &'a File<'a>>>,
    rules: RefCell<RefSet<'a, Rule<'a>>>,

    statuses: StatusMap<RefCell<RefSet<'a, Rule<'a>>>>,

    facfiles_used: RefCell<RefSet<'a, File<'a>>>,
    root: PathBuf,
}

impl<'a> Build<'a> {
    /// Create the arenas to give to `Build::new`
    pub fn arenas() -> (typed_arena::Arena<File<'a>>,
                        typed_arena::Arena<Rule<'a>>) {
        (typed_arena::Arena::new(),
         typed_arena::Arena::new())
    }
    /// Construct a new `Build`.
    ///
    /// # Examples
    ///
    /// ```
    /// use fac::build;
    /// let arenas = build::Build::arenas();
    /// let b = build::Build::new(&arenas);
    /// ```
    pub fn new(allocators: &'a (typed_arena::Arena<File<'a>>,
                                typed_arena::Arena<Rule<'a>>)) -> Build<'a> {
        let root = std::env::current_dir().unwrap();
        let b = Build {
            alloc_files: &allocators.0,
            alloc_rules: &allocators.1,
            files: RefCell::new(HashMap::new()),
            rules: RefCell::new(RefSet::new()),
            statuses: StatusMap::new(|| RefCell::new(RefSet::new())),
            facfiles_used: RefCell::new(RefSet::new()),
            root: root,
        };
        for ref f in git::ls_files() {
            b.new_file_private(f, true);
            println!("i see {:?}", f);
        }
        b
    }
    fn new_file_private<P: AsRef<Path>>(&self, path: P,
                                        is_in_git: bool)
                                        -> &File<'a> {
        // If this file is already in our database, use the version
        // that we have.  It is an important invariant that we can
        // only have one file with a given path in the database.
        match self.files.borrow().get(path.as_ref()) {
            Some(f) => return f,
            None => ()
        }
        let f = self.alloc_files.alloc(File {
            rule: RefCell::new(None),
            path: PathBuf::from(path.as_ref()),
            children: RefCell::new(RefSet::new()),
            rules_defined: RefCell::new(RefSet::new()),
            kind: Cell::new(None),
            is_in_git: is_in_git,
        });
        self.files.borrow_mut().insert(& f.path, f);
        f
    }

    /// Look up a `File` corresponding to a path, or if it doesn't
    /// exist, allocate space for a new `File`.
    ///
    /// # Examples
    ///
    /// ```
    /// use fac::build;
    /// let arenas = build::Build::arenas();
    /// let mut b = build::Build::new(&arenas);
    /// let t = b.new_file("test");
    /// ```
    pub fn new_file<P: AsRef<Path>>(&self, path: P) -> &File<'a> {
        self.new_file_private(path, false)
    }

    /// Allocate space for a new `Rule`.
    pub fn new_rule(&self,
                    command: &OsStr,
                    working_directory: &Path,
                    facfile: &'a File<'a>,
                    cache_suffixes: HashSet<OsString>,
                    cache_prefixes: HashSet<OsString>)
                    -> &Rule<'a> {
        let r = self.alloc_rules.alloc(Rule {
            inputs: RefCell::new(vec![]),
            outputs: RefCell::new(vec![]),
            status: Cell::new(Status::Unknown),
            cache_prefixes: cache_prefixes,
            cache_suffixes: cache_suffixes,
            working_directory: PathBuf::from(working_directory),
            facfile: facfile,
            command: OsString::from(command),
        });
        self.statuses[Status::Unknown].borrow_mut().insert(r);
        self.rules.borrow_mut().insert(r);
        r
    }

    /// Read a fac file
    pub fn read_file(&self, file: &File<'a>) -> std::io::Result<()> {
        let mut f = std::fs::File::open(&file.path)?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        let mut command: Option<&[u8]> = None;
        let mut cache_prefixes = HashSet::new();
        let mut cache_suffixes = HashSet::new();
        for (lineno_minus_one, line) in v.split(|c| *c == b'\n').enumerate() {
            let lineno = lineno_minus_one + 1;
            let parse_error = |msg: &str| {
                Err(std::io::Error::new(std::io::ErrorKind::Other,
                                        format!("error: {:?}:{}: {}",
                                                file.pretty_path(self), lineno, msg)))
            };
            if line.len() < 2 || line[0] == b'#' { continue };
            if line[1] != b' ' {
                return parse_error("Second character of line should be a space.");
            }
            match line[0] {
                b'|' => {
                    match command {
                        None => (),
                        Some(c) => {
                            self.new_rule(OsStr::from_bytes(c),
                                          file.path.parent().unwrap(),
                                          file,
                                          cache_suffixes,
                                          cache_prefixes);
                            cache_prefixes.clear();
                            cache_suffixes.clear();
                        }
                    }
                    command = Some(&line[2..]);
                },
                _ => return parse_error(&format!("Invalid first character: {:?}", line[0])),
            }
            println!("Line {}: {:?}", lineno, line);
        }
        Ok(())
    }
}
