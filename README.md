# Survive

Survive is an experimental implementation of [system prevalence](https://en.wikipedia.org/wiki/System_prevalence), an extremely simple model for object persistence. It works by *snapshotting your entire data occasionally* and maintaining an *append-only journal* of data mutations (commonly known as *transactions*) in the meantime.

In many practical applications, system prevalence can replace the need for full-fledged databases of all sorts (key-value, relational, etc.).

All you need:

* A plain old Rust data type that is:
  * Serializable using [Serde](http://serde.rs/).
  * Only modifiable through serializable mutations.
* A blank directory on your file system.

That's it! Here's what you don't have to deal with:

* Dedicated server processes
* Special setup
* Proprietary query language
* Constraints on the algorithms, data structures, or indexes you use
* Limited constraint models (triggers, foreign keys, etc.)

All of your domain logic is implemented in vanilla Rust. All you have to do is make sure your data is serializable and your data mutations are well-defined and deterministic!

There are some catches, of course:

* Most notably, the entire data must fit in RAM.
* There is no "schema" abstraction, so there is no e.g. `CREATE TABLE` or `ALTER TABLE` (from SQL). As such, systematic data type migration is currently outside the scope of this library. There are probably ways to help alleviate this practical pain point, but I haven't researched them myself.
* For a plethora of other pitfalls and trade-offs, see [further reading](#further-reading).

For a technical explanation of Survive, please refer to the source code's documentation. The code is lightweight and the details are simple.

Survive uses [CBOR](https://github.com/pyfisch/cbor) under the hood for serialization.

## Full example

```toml
[dependencies]
survive = "0.1"
serde = "1.0"
serde_derive = "1.0"
```

```rust
extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate survive;

use survive::{Survive, Survivable, Mutation};
use std::collections::BTreeSet;

#[derive(Default, Deserialize, Serialize)]
struct Model {
    // You can use whatever data structure you want—as long as it's serializable!
    values: BTreeSet<String>,
}

impl Survivable for Model { }

#[derive(Deserialize, Serialize)]
enum ModelMutation {
    // Add a value to the set.
    Add(String),
    // Remove a value to the set.
    Remove(String),
}


impl Mutation<Model> for ModelMutation {
    type Result = ();

    // The implementation of this function **must be deterministic**!
    fn mutate(self, data: &mut Model) {
        match self {
            ModelMutation::Add(ref value) => { data.values.insert(value.clone()); },
            ModelMutation::Remove(ref value) => { data.values.remove(value); },
        }
    }
}

fn main() {
    // Create a new home for some data (assuming the specified directory does not yet exist):
    let mut data = Survive::<Model>::new("path/to/some-directory").unwrap();
    data.mutate(FooMutation::Add("Hello!".to_string())).unwrap();
    data.mutate(FooMutation::Add("World!".to_string())).unwrap();
    assert_eq!(data.get().values.contains("Hello!"));
    data.mutate(FooMutation::Remove("Hello!".to_string())).unwrap();
    // The system is closed on drop:
    drop(data);

    // Open the persisted data again:
    let data = Survive::<Model>::new("path/to/some-directory").unwrap();
    assert_eq!(!data.get().values.contains("Hello!"));
    assert_eq!(data.get().values.contains("World!"));
}
```

## Performance

Some quick numbers: On my reasonably powered development computer I am able to create and persist a `BTreeSet<String>` with 1,000,000 individually-added strings in ~750ms, where each string is approximately 6 bytes long like in the above example. The resultant snapshot file is ~7 MB in size. Re-loading the snapshot takes about ~500ms.

The current architecture seems to scale reasonably well to data approaching 1 GB, but some performance tuning (see `survive::Options`) is necessary. Most problematic is the automatic compaction (full-data snapshotting) that Survive does by default, which occurs after a certain number of journaled mutations and blocks execution. There's a trade-off here between 1) start-up time and journal file length, and 2) occasional runtime pauses to save data snapshots.

In my view, the strongest benefit of this architecture is that data reads are virtually free of overhead—you're just accessing plain-old Rust data structures. As such, the analogous ["full table scans"](https://en.wikipedia.org/wiki/Full_table_scan) are not nearly so scary, and data indexes can be catered exactly to your needs.

## Further reading

System prevalence has been around since at least 2001 with Klaus Wuestefeld's [Prevayler](http://prevayler.org/), an implementation of the pattern in Java. The simplicity of this architectural model is quite appealing. However, there are trade-offs to consider when adopting it, which can be reviewed on the legendary [wiki.c2.com](http://wiki.c2.com/?PrevalenceLayer). Even if the discussion is quite old, most of the points seem to me to still be relevant today.

* [Object Prevalence: An In-Memory, No-Database Solution to Persistence](https://medium.com/@paul_83250/object-prevalence-an-in-memory-no-database-solution-to-persistence-a1ebcd1493b0)
* [Prevayler's old wiki](http://prevayler.org/old_wiki/Welcome.html)
