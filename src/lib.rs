extern crate byteorder;
extern crate serde_cbor;
extern crate serde;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write, BufReader, BufWriter},
    path::{Path, PathBuf},
};

type MutationTypes<T> = BTreeMap<u32, Box<Fn(&[u8], &mut T) -> Result<(), Error>>>;

/// Used as the entry point into the library and to construct a `Survive` instance.
pub struct Builder<T: Survivable> {
    options: Options,
    mutation_types: MutationTypes<T>,
}

impl<T: Survivable> Builder<T> {
    /// Begins the process of constructing a `Survive` instance.
    pub fn new() -> Builder<T> {
        Builder {
            options: Options::default(),
            mutation_types: MutationTypes::new(),
        }
    }

    /// Creates the `Survive` instance.
    ///
    /// This opens and synchronizes with a persisted instance of the data type `T`. If a persistence
    /// does not yet exist, it is created using the `Default` implementation of `T`.
    ///
    /// The given `path` is a path to a directory that will contain all of the necessary files to
    /// persist the data. If the directory or any of its parents do not exist, they will be
    /// recursively created.
    pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<Survive<T>, Error> {
        Survive::open(path, self)
    }

    /// Registers a mutation type to be used when restoring the data from its journal file.
    ///
    /// # Panics
    ///
    /// Panics if a mutation type already exists with the given `Mutation::ID`.
    pub fn register<M: Mutation<T>>(&mut self) -> &mut Builder<T> {
        if self.mutation_types.contains_key(&M::ID) {
            panic!("Mutation already registered with ID {}", M::ID);
        }

        self.mutation_types.insert(M::ID, Box::new(|buf, data| {
            let mutation: M = serde_cbor::from_slice(buf)?;
            mutation.mutate(data);
            Ok(())
        }));
        self
    }

    /// The limit of the journal file length (in bytes). When the length exceeds this value,
    /// `Survive` will automatically compact the state file and clear the journal.
    ///
    /// If this is `None`, automatic compaction is disabled. If this is `Some(0)`, compaction runs
    /// after every data mutation.
    ///
    /// By default, this is set to 10 MB (10,485,760 bytes).
    pub fn max_journal_file_length(&mut self, max: Option<usize>) -> &mut Builder<T> {
        self.options.max_journal_file_length = max;
        self
    }

    /// By default, Writing to the journal file is buffered (via `BufWriter`). This improves
    /// mutation performance significantly (in some experiments, by approximately an order of
    /// magnitude), but comes at a disadvantage. Because serialized mutations are not flushed to the
    /// journal file immediately, abnormal program closure can cause the mutations to not be
    /// journaled and thus lost forever.
    ///
    /// At the time of writing, we use `BufWriter`'s default of an 8 KB buffer. So, for reasonably
    /// small transactions (in terms of their serialized bytes), a crash can result in approximately
    /// 8 KB of transactions to be lost. In the case of a large transaction that exceeds 8 KB in
    /// serialized size, the entire large transaction can be lost.
    ///
    /// To disable journal file write buffering, set this to `false`.
    pub fn use_journal_buffer(&mut self, use_it: bool) -> &mut Builder<T> {
        self.options.use_journal_buffer = use_it;
        self
    }
}

/// A representation of a persistable data type.
///
/// All you need:
///
/// * A data type that is:
///   * Serializable/deserializable through Serde. If the type or its serialization implementation
///     changes, it is your responsibility to migrate your data to the new type. `Survive`'s will
///     fail if there is a serialization mismatch.
///   * Only modifiable through serializable/deserializable mutations. You need the discipline to
///     not subvert this constraint (via interior mutability, inconsistent serialization,
///     non-deterministic behavior, etc.), otherwise you will likely end up with invalid data
///     mutations and thus inconsistent/unexpected data. Like the main data type, mutation types
///     must also be consistently serializable.
/// * A blank directory on your file system. Do not manually modify the contents of this directory!
///   But feel free to move, copy, or delete it as needed.
///
/// # Technical description
///
/// ## Files
///
/// There are at most three files in the persistence directory at any given time:
///
/// * **State file**: This is the file that contains the a complete serialization of the data at
///   some point.
/// * **Journal file**: This file contains a serialized list of mutations made to the data since the
///   time of the state file's creation.
/// * **Transitional state file**: This file exists temporarily during the journal compaction phase,
///   and is meant to replace the state file once compaction is complete.
///
/// ## Compaction
///
/// In order to prevent the journal file from indefinitely growing in length, it is occasionally
/// cleared and the state file is recreated from scratch during the **compaction phase**. This phase
/// is trigged when the journal file exceeds 10 MB, and also occurs during startup (regardless of
/// journal file size).
///
/// While compaction is occurring, a write lock is placed on the data so that intermediate data
/// modifications are not at risk of not being recorded in the journal. Hopefully this locking
/// period is not a considerable burden. Alternatively, locking could be prevented by creating a
/// temporary clone of the data in memory so that mutations of the main data can continue
/// uninterrupted. In this case, a transitional journal is also necessary. The costs of this method
/// are: 1) the data type has to implement `Clone`, and 2) twice the memory is required.
pub struct Survive<T: Survivable> {
    path: PathBuf,
    data: T,
    journal: BufWriter<File>,
    journal_file_length: usize,
    options: Options,
}

impl<T: Survivable> Survive<T> {
    /// Returns an immutable reference to the underlying data.
    pub fn get(&self) -> &T {
        &self.data
    }

    /// Performs a mutation on the underlying data.
    pub fn mutate<M: Mutation<T>>(&mut self, mutation: M) -> Result<M::Result, Error> {
        fn write_buf(w: &mut Write, mutation_id: u32, buf: &[u8]) -> Result<(), Error> {
            w.write_u32::<LittleEndian>(mutation_id)?;
            w.write_u32::<LittleEndian>(buf.len() as u32)?;
            w.write_all(buf.as_ref())?;
            Ok(())
        }

        let buf = serde_cbor::to_vec(&mutation)?;

        let write_result = if self.options.use_journal_buffer {
            write_buf(&mut self.journal, M::ID, buf.as_ref())
        } else {
            write_buf(&mut self.journal, M::ID, buf.as_ref()).and_then(|_| {
                Ok(self.journal.flush()?)
            })
        };

        // If writing fails, the journal file may be corrupted and compaction should be triggered
        // immediately.
        if write_result.is_err() {
            self.compact()?;
        } else {
            self.journal_file_length += 4 + buf.len();
            if let Some(max) = self.options.max_journal_file_length {
                if self.journal_file_length > max {
                    self.compact()?;
                }
            }
        }

        Ok(mutation.mutate(&mut self.data))
    }

    /// Returns the current length of the journal file in bytes.
    pub fn journal_file_length(&self) -> usize {
        self.journal_file_length
    }

    // Force a compaction of the journal file into the state file.
    pub fn compact(&mut self) -> Result<(), Error> {
        let state_path = self.path.join("state");
        let transitional_state_path = self.path.join("state~");

        let mut transitional_state = BufWriter::new(File::create(&transitional_state_path)?);
        serde_cbor::to_writer(&mut transitional_state, &self.data)?;

        if state_path.exists() {
            fs::remove_file(&state_path)?;
        }

        self.journal.flush()?;
        self.journal.get_ref().set_len(0)?;
        self.journal_file_length = 0;

        fs::rename(&transitional_state_path, &state_path)?;

        Ok(())
    }

    fn open<P: AsRef<Path>>(path: P, builder: &Builder<T>) -> Result<Survive<T>, Error> {
        let path = path.as_ref().to_path_buf();
        let data = Self::load(path.as_ref(), &builder.mutation_types)?
            .unwrap_or_else(|| {
                let mut data = T::default();
                data.state_loaded();
                data
            });
        let journal_path = path.join("journal");
        let journal_file = fs::OpenOptions::new().append(true).create(true).open(&journal_path)?;
        let journal = BufWriter::new(journal_file);
        let options = builder.options.clone();
        let mut result = Survive { path, data, journal, journal_file_length: 0, options };
        result.compact()?;
        Ok(result)
    }

    fn load(path: &Path, mutation_types: &MutationTypes<T>) -> Result<Option<T>, Error> {
        fs::create_dir_all(path)?;

        let state_path = path.join("state");
        let journal_path = path.join("journal");
        let transitional_state_path = path.join("state~");

        if !state_path.exists() {
            if journal_path.exists() {
                // The program previously crashed right after deleting the main state file and right
                // before deleting the journal file.
                fs::remove_file(&journal_path)?;
            }

            if transitional_state_path.exists() {
                // The program previously crashed right after deleting the main state file (and the
                // journal file) and right before renaming the transitional state file.
                fs::rename(&transitional_state_path, &state_path)?;
            } else {
                return Ok(None);
            }
        }

        if transitional_state_path.exists() {
            fs::remove_file(&transitional_state_path)?;
        }

        let mut data: T = serde_cbor::from_reader(BufReader::new(File::open(state_path)?))?;
        data.state_loaded();
        if journal_path.exists() {
            let mut journal = BufReader::new(File::open(journal_path)?);
            loop {
                let mutation_id = match ignore_eof(journal.read_u32::<LittleEndian>())? {
                    Some(mutation_id) => mutation_id,
                    None => break,
                };

                let length = match ignore_eof(journal.read_u32::<LittleEndian>())? {
                    Some(length) => length as usize,
                    None => break,
                };

                let mut buf = vec![0; length];
                if let None = ignore_eof(journal.read_exact(buf.as_mut_slice()))? {
                    break;
                }

                match mutation_types.get(&mutation_id) {
                    Some(process) => { process(&buf, &mut data)?; },
                    None => return Err(Error::UnregisteredMutation { id: mutation_id }),
                }
            }
        }

        Ok(Some(data))
    }
}

fn ignore_eof<T>(result: Result<T, io::Error>) -> Result<Option<T>, Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) => {
            if let io::ErrorKind::UnexpectedEof = err.kind() {
                // The program previously crashed in the middle of writing the chunk.
                Ok(None)
            } else {
                Err(err.into())
            }
        }
    }
}

/// A type that can be persisted by `Survive`.
pub trait Survivable: Default + Serialize + DeserializeOwned {
    /// Called when the state has been loaded from file, but before the journal has been processed.
    /// If a persisted data file does not exist, this is called anyway.
    fn state_loaded(&mut self) { }
}

/// A serializable change to the `Survivable` data.
pub trait Mutation<T: Survivable>: Serialize + DeserializeOwned {
    // A unique identifier that is used to reference this type (see `Builder::register`).
    const ID: u32;

    // The type returned by `Mutation::mutate`.
    type Result;

    /// Commits a change to the data.
    ///
    /// # Determinism
    ///
    /// It is up to the programmer to **make sure that this implementation is side effect free**,
    /// i.e. **deterministic**. Executions of the function must produce the exact same data mutation
    /// each time.
    ///
    /// Common accidental violations of this rule include:
    ///
    /// * Depending on external resources such as files or network endpoints
    /// * Generating dates or timestamps in the function
    /// * Allocating random numbers or IDs
    ///
    /// These violations produce different results on subsequent "replays" (i.e. when the journal
    /// file is processed).
    fn mutate(self, data: &mut T) -> Self::Result;
}

#[derive(Clone)]
struct Options {
    max_journal_file_length: Option<usize>,
    use_journal_buffer: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            max_journal_file_length: Some(10_485_760),
            use_journal_buffer: true,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// A serialization or deserialization error.
    Cbor(serde_cbor::error::Error),
    /// A system I/O error.
    Io(io::Error),
    /// An unrecognized mutation was encountered while reading the journal file.
    UnregisteredMutation {
        id: u32,
    },
}

impl From<serde_cbor::error::Error> for Error {
    fn from(err: serde_cbor::error::Error) -> Error {
        Error::Cbor(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}
