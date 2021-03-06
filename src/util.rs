//! This module describes a set of utilities that will be used throughout the other modules

extern crate crossbeam_deque;
extern crate digest;
extern crate hex;
extern crate md5;
extern crate regex;
extern crate sha1;
extern crate sha2;

#[cfg(unix)]
extern crate termios;

#[cfg(windows)]
extern crate winapi;

use self::regex::Regex;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Error, Read};
use std::path::PathBuf;

use self::digest::{Digest, DynDigest};
use self::md5::Md5;
use self::sha1::Sha1;
use self::sha2::{Sha224, Sha256, Sha384, Sha512};

use self::crossbeam_deque::{Injector, Steal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use std::fmt;

/// The mode the program will operate in
#[derive(Debug, Clone)]
pub enum Mode {
    Filter,
    Update,
    Verify,
}

/// The level of detail the program will be logging
#[derive(Debug, PartialEq, Clone)]
pub enum LogLevel {
    Quiet,
    Info,
    Progress,
    Debug,
}

/// A structure that defines everything needed to hash a requested file and return the result
pub struct HashTask {
    /// Path to the file that should be hashed
    pub path: String,
    /// The desired working directory of the worker thread
    pub workdir: PathBuf,
    /// A reference to an Options struct containing various parameters
    pub opts: Arc<Options>,
    /// A string containing the hash that the file should match
    pub cmp: String,
    /// A channel to return the calculated hash and cmp to the task generator
    pub result_chan: Sender<Result<(String, String), HashError>>,
}

/// An error that occurs when a file cannot be hashed
#[derive(Debug)]
pub struct HashError {
    source: io::Error,
    path: String,
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.source.to_string(), self.path)
    }
}

/// A single structure that gets constructed by commandline arguments and describes the behavior of the program
#[derive(Debug, Clone)]
pub struct Options {
    /// Whether or not the help message will be displayed
    pub help: bool,
    /// Whether or not to display version information
    pub version_info: bool,
    /// My name
    pub program_name: String,
    /// The hashing algorithm to use
    pub algorithm: String,
    /// Whether or not it will be operated on a single folder or every subfolder
    pub subdir_mode: bool,
    /// The mode the program will operate in
    pub mode: Mode,
    /// The level of detail the program will be logging
    pub log_level: LogLevel,
    /// Maximum number of threads to spawn
    pub num_threads: usize,
    /// The folder to operate on
    pub folder: String,
}

impl Options {
    /// Creates a new instance of Options containing all settings given through the commandline
    ///
    /// # Arguments
    ///
    /// * `args` - A vec of Strings containing all commandline parameters
    pub fn new(args: Vec<String>) -> Options {
        let mut opts = Options {
            help: false,
            version_info: false,
            program_name: args[0].to_string(),
            algorithm: "sha1".to_string(),
            subdir_mode: false,
            mode: Mode::Filter,
            log_level: LogLevel::Info,
            num_threads: 0,
            folder: ".".to_string(),
        };

        // prepare Strings for parsing
        let args = prepare_args(args[1..].to_vec());

        // loop through every argument, except the name
        for i in 0..args.len() {
            let arg = &args[i];

            // match options (Strings with leading -)
            if arg.starts_with('-') {
                match arg.as_ref() {
                    "-a" | "--algo" | "--algorithm" => {
                        opts.algorithm = args[i + 1].clone().to_lowercase()
                    }
                    "-s" | "--subdir" | "--subdirs" | "--subdirectories" => opts.subdir_mode = true,
                    "-u" | "--update" => opts.mode = Mode::Update,
                    "-v" | "--verify" => opts.mode = Mode::Verify,
                    "--loglevel" | "--log_level" | "--log-level" => {
                        opts.log_level = {
                            match args
                                .get(i + 1)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "Usage: {} {} quiet/info/debug",
                                        opts.program_name, args[i]
                                    )
                                })
                                .as_ref()
                            {
                                "none" | "quiet" | "0" => LogLevel::Quiet,
                                "info" | "1" => LogLevel::Info,
                                "progress" => LogLevel::Progress,
                                "debug" | "2" => LogLevel::Debug,
                                _ => LogLevel::Info,
                            }
                        }
                    }
                    "--quiet" => opts.log_level = LogLevel::Quiet,
                    "-T" | "--threads" => {
                        opts.num_threads = args
                            .get(i + 1)
                            .unwrap_or_else(|| {
                                panic!("Usage: {} -T NUMBER_OF_MAX_THREADS", opts.program_name)
                            })
                            .trim()
                            .parse()
                            .unwrap_or_else(|_| {
                                panic!("Usage: {} -T NUMBER_OF_MAX_THREADS", opts.program_name)
                            })
                    }
                    "-h" | "--help" => opts.help = true,
                    "-V" | "--version" => opts.version_info = true,
                    _ => opts.help = true,
                }
            } else {
                // if a String does not start with - and the String before it is none of the below, it is the folder to operate on
                match args[i - 1].as_ref() {
                    "--loglevel" | "--log_level" | "--log-level" | "-a" | "--algo"
                    | "--algorithm" | "-T" | "--threads" => {}
                    _ => opts.folder = arg.clone(),
                }
            }
        }

        opts
    }

    /// Indicates that the program is in the debug loglevel
    pub fn loglevel_debug(&self) -> bool {
        self.log_level == LogLevel::Debug
    }

    /// Indicates that the program is at least in the info loglevel
    pub fn loglevel_info(&self) -> bool {
        self.log_level == LogLevel::Debug || self.log_level == LogLevel::Info
    }

    /// Indicates that the program is in the progress loglevel
    pub fn loglevel_progress(&self) -> bool {
        self.log_level == LogLevel::Progress
    }
}

/// Prepares a vec of Strings for parsing options
///
/// A new vec gets returned that contains more Strings than the original, because two rules get applied:
/// * If a String starts with a single -, but it has more than 2 characters, the parameters get split
///   into single Strings with a leading -
/// * If a String contains a =, the = will get cut and the prefix and suffix will be split into two Strings
/// This is necessary for the match statement in Options::new to work correctly
///
/// # Arguments
///
/// * `args` A vec of Strings containing all commandline parameters
fn prepare_args(args: Vec<String>) -> Vec<String> {
    let mut prepared_args = Vec::with_capacity(args.len());

    for arg in args {
        if !arg.contains('=') {
            if arg.contains('-') && !arg.contains("--") && arg.len() > 2 {
                let characters = &arg[1..];
                for char in characters.chars() {
                    let single_arg = format!("-{}", char);
                    prepared_args.push(single_arg);
                }
            } else {
                prepared_args.push(arg);
            }
        } else {
            let position = arg.find('=').unwrap();
            let prefix = arg[0..position].to_string();
            let suffix = arg[position + 1..].to_string();
            prepared_args.push(prefix);
            prepared_args.push(suffix);
        }
    }

    prepared_args
}

/// Creates a regex that identifies hashsum and path from a hashsum line.
///
/// # Arguments
/// * `opts` Options object that contains the desired algorithm
pub fn regex_from_opts(opts: &Options) -> Result<Regex, &'static str> {
    match opts.algorithm.as_ref() {
        "sha1" => Ok(Regex::new(r"([[:xdigit:]]{40})\s\s(.*)$").unwrap()),
        "md5" => Ok(Regex::new(r"([[:xdigit:]]{32})\s\s(.*)$").unwrap()),
        "sha224" => Ok(Regex::new(r"([[:xdigit:]]{56})\s\s(.*)$").unwrap()),
        "sha256" => Ok(Regex::new(r"([[:xdigit:]]{64})\s\s(.*)$").unwrap()),
        "sha384" => Ok(Regex::new(r"([[:xdigit:]]{96})\s\s(.*)$").unwrap()),
        "sha512" => Ok(Regex::new(r"([[:xdigit:]]{128})\s\s(.*)$").unwrap()),
        _ => Err("Could not recognize hashing algorithm"),
    }
}

/// Imitate _algorithm_sum with the path of a file to get the hashsum.
///
/// # Arguments
///
/// * `path` Path to the file to be hashed, relative to the workdir
/// * `workdir` Path to the wanted working directory
/// * `opts` A reference to an Options object containing information about the program behavior
///
/// # Returns
///
/// A String containing the output of the _algorithm_sum command.
pub fn calculate_hash(
    path: String,
    workdir: &PathBuf,
    opts: &super::util::Options,
) -> Result<String, HashError> {
    let file = fs::File::open(&format!("{}/{}", workdir.to_str().unwrap(), path));
    const BUFFER_SIZE: usize = 1024;
    let mut buffer = [0; BUFFER_SIZE];

    let mut hasher = match opts.algorithm.as_ref() {
        "sha1" => Box::new(Sha1::new()) as Box<dyn DynDigest>,
        "md5" => Box::new(Md5::new()) as Box<dyn DynDigest>,
        "sha224" => Box::new(Sha224::new()) as Box<dyn DynDigest>,
        "sha256" => Box::new(Sha256::new()) as Box<dyn DynDigest>,
        "sha384" => Box::new(Sha384::new()) as Box<dyn DynDigest>,
        "sha512" => Box::new(Sha512::new()) as Box<dyn DynDigest>,
        _ => panic!("Algorithm not recognized"),
    };

    match file {
        Err(e) => {
            return Err(HashError {
                source: e,
                path: path,
            })
        }
        Ok(mut file) => loop {
            let n = file.read(&mut buffer).unwrap();
            hasher.input(&buffer[0..n]);

            if n == 0 || n < BUFFER_SIZE {
                break;
            }
        },
    }

    Ok(format!("{}  {}\n", hex::encode(hasher.result()), path))
}

/// Starts a number of worker threads ready for hashing files.
///
/// # Arguments
///
/// * `num_threads` Number of worker threads to start
/// * `q` Reference to the Injector that will carry the HashTask objects
/// * `produced_finished` Reference to a central boolean that indicates that no new HashTasks will be pushed into q
/// * `worker_handles` A mutable reference to a vector of thread handles, in which the handles to the worker threads will be stored
pub fn execute_workers(
    num_threads: usize,
    q: Arc<Injector<super::util::HashTask>>,
    producer_finished: Arc<AtomicBool>,
    worker_handles: &mut Vec<JoinHandle<()>>,
) {
    for _ in 0..num_threads {
        let myq = Arc::clone(&q);
        let myp = Arc::clone(&producer_finished);

        let handle = std::thread::spawn(move || loop {
            let task = myq.steal();

            match task {
                Steal::Success(task) => {
                    let hashline = calculate_hash(task.path, &task.workdir, &task.opts);
                    match hashline {
                        Ok(hashline) => task.result_chan.send(Ok((hashline, task.cmp))).unwrap(),
                        Err(e) => task.result_chan.send(Err(e)).unwrap(),
                    };
                }
                Steal::Retry => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Steal::Empty => {
                    if myp.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        });

        worker_handles.push(handle);
    }
}

/// Disables echo on terminal
#[cfg(unix)]
pub fn terminal_noecho() {
    let mut termios_noecho = termios::Termios::from_fd(0).unwrap();
    termios_noecho.c_lflag &= !termios::ECHO;
    termios::tcsetattr(0, termios::TCSANOW, &termios_noecho).unwrap();
}

/// Disables echo on terminal
#[cfg(windows)]
pub fn terminal_noecho() {
    use self::winapi::shared::minwindef::LPDWORD;
    use self::winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
    use self::winapi::um::processenv::GetStdHandle;
    use self::winapi::um::winbase::STD_INPUT_HANDLE;
    use self::winapi::um::wincon::ENABLE_ECHO_INPUT;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };

    let mut mode = 0;
    // unsafe { GetConsoleMode(handle, &mut mode as LPDWORD) };
    unsafe { GetConsoleMode(handle, &mut mode as LPDWORD) };
    unsafe { SetConsoleMode(handle, mode & (!ENABLE_ECHO_INPUT)) };
}

/// Read paths line by line from a file and return them in a Vector
///
/// # Arguments
///
/// * `filepath` Path to the file to be read
pub fn read_paths_from_file(filepath: &str) -> Vec<PathBuf> {
    let mut vec = Vec::new();

    let file = OpenOptions::new().read(true).open(filepath);
    if let Ok(file) = file {
        let reader = BufReader::new(file);
        for line in reader.lines() {
            if let Ok(line) = line {
                vec.push(PathBuf::from(line));
            }
        }
    }

    vec
}

/// An Object that returns Paths to all the files in all folders recursively (like find)
///
/// DirWalker implements Iterator and Read for this behavior
pub struct DirWalker {
    /// A Buffer for the currently known files
    current_files: Vec<PathBuf>,
    /// A Buffer for the directories that have to be scanned recursively
    current_directories: Vec<PathBuf>,
    /// A Buffer for the filepath that was only partially read
    unfinished_read: String,
    /// Whether or not the first directory should be stripped from the filepath
    subdir_mode: bool,
}

impl DirWalker {
    /// Create a new DirWalker object
    ///
    /// # Arguments
    ///
    /// * `start_directory` Path to the directory that should be scanned
    /// * `subdir_mode` Whether or not the first directory should be stripped from the filepath
    pub fn new(start_directory: &PathBuf, subdir_mode: bool) -> DirWalker {
        let mut dirwalker = DirWalker {
            current_files: Vec::new(),
            current_directories: Vec::new(),
            unfinished_read: String::new(),
            subdir_mode,
        };

        dirwalker.populate_with_dir(&start_directory);

        dirwalker
    }

    /// Update the DirWalker object by adding all subdirectories and files of directory to the queue
    ///
    /// # Arguments
    ///
    /// * `directory` Path to the directory that is going to be scanned
    fn populate_with_dir(&mut self, directory: &PathBuf) {
        let dir_entries = fs::read_dir(directory);

        if let Ok(dir_entries) = dir_entries {
            let mut files = Vec::new();
            let mut dirs = Vec::new();

            for entry in dir_entries {
                let entry = entry.unwrap();
                let metadata = entry.metadata().unwrap();

                if metadata.is_dir() {
                    dirs.push(entry.path());
                }
                if metadata.is_file() {
                    files.push(entry.path());
                }
            }

            self.current_directories.append(&mut dirs);
            self.current_files.append(&mut files);
        }
    }

    /// Return the position of the first directory seperator in a str containing a path
    ///
    /// # Arguments
    ///
    /// * `path_string` String containing the path to be searched
    #[cfg(unix)]
    fn find_dir_seperator_position(path_string: &str) -> usize {
        path_string.find('/').unwrap()
    }

    /// Return the position of the first directory seperator in a str containing a path
    ///
    /// # Arguments
    ///
    /// * `path_string` String containing the path to be searched
    #[cfg(windows)]
    fn find_dir_seperator_position(path_string: &str) -> usize {
        path_string.find('\\').unwrap()
    }
}

impl Iterator for DirWalker {
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.current_files.is_empty() {
            let filepath = self.current_files.pop().unwrap();

            if self.subdir_mode {
                let path_string = filepath.to_string_lossy().to_string();
                let path_string = &path_string[2..];
                let position = DirWalker::find_dir_seperator_position(path_string);
                let path_string = format!(".{}", path_string[position..].to_string());
                let filepath = PathBuf::from(path_string);
                return Some(filepath);
            }

            return Some(filepath);
        }

        if !self.current_directories.is_empty() {
            let dirpath = self.current_directories.pop().unwrap();

            self.populate_with_dir(&dirpath);

            return self.next();
        }

        None
    }
}

impl Read for DirWalker {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let mut i = 0;

        if !self.unfinished_read.is_empty() {
            loop {
                if i >= buf.len() || i >= self.unfinished_read.len() {
                    break;
                }

                buf[i] = self.unfinished_read.as_bytes()[i];
                i += 1;
            }

            if i < self.unfinished_read.len() {
                self.unfinished_read = self.unfinished_read[0..i].to_string();
            }

            return Ok(i);
        }

        let path: Option<PathBuf> = self.next();

        match path {
            None => Ok(0),
            Some(path) => {
                let path_str = format!("{}\n", path.to_str().unwrap());

                loop {
                    if i >= buf.len() || i >= path_str.len() {
                        break;
                    }

                    buf[i] = path_str.as_bytes()[i];
                    i += 1;
                }

                if i < path_str.len() {
                    self.unfinished_read = path_str[0..i].to_string();
                }

                Ok(i)
            }
        }
    }
}
