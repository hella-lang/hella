# Hella Standard Library Overhaul

## Mission

You are responsible for a **complete audit, redesign, and implementation of Hella's standard library**.

Hella's current standard library is incomplete and contains APIs that are inconsistent, oddly designed, overly narrow, or not particularly useful. Your job is to turn it into a **serious, cohesive, practical, cross-platform standard library** suitable for real Hella programs.

This is not a task to merely add a few missing functions.

You must:

1. Audit Hella's current standard library.
2. Study the **actual installed source code, APIs, documentation, and examples** of:

   * Rust's standard library
   * Odin's standard/core libraries
   * Go's standard library
3. Identify what those ecosystems expose, why they expose it, and which ideas are worth adopting.
4. Design an Hella-native standard library rather than blindly copying another language.
5. Rewrite, reorganize, remove, replace, and expand Hella's standard library as necessary.
6. Treat **strings, arrays, vectors, and maps as core foundational facilities**, not incidental utilities.
7. Modify the compiler/runtime when the current language implementation prevents a clean standard-library design.
8. Preserve existing Hella language semantics and avoid breaking unrelated functionality.
9. Make the result as cross-platform as realistically possible across:

   * Windows
   * Linux
   * macOS
10. Add comprehensive tests and examples for every substantial API.
11. Leave the repository in a coherent, buildable, documented state.

The goal is:

> **Hella's standard library should be something a Hella programmer can actually build software with.**

Do not optimize for API count.

Optimize for **usefulness, coherence, correctness, portability, ergonomics, consistency, and discoverability**.

---

# 1. Non-Negotiable Research Rule

Do **not** rely on your memory of Rust, Odin, or Go.

Inspect the actual installations available on the machine.

First determine where the toolchains and their sources live.

Use commands appropriate to the environment, such as:

```sh
which rustc
rustc --print sysroot

which cargo

which go
go env GOROOT GOPATH GOOS GOARCH

which odin
odin root
```

Then inspect the relevant source trees directly.

For Rust, study the installed standard library sources and documentation.

For Go, study the actual `GOROOT/src` tree.

For Odin, study its installed/core library source tree.

Do not merely search for function names.

Study:

* package/module organization
* naming
* type design
* error handling
* ownership/lifetime implications where relevant
* abstractions
* portability decisions
* OS-specific implementations
* common usage patterns
* API boundaries
* what belongs in the standard library versus what does not
* which seemingly large libraries are actually small and practical
* which APIs are commonly composed together

Use these ecosystems as **design references**, not as specifications.

Never copy another language's API blindly.

---

# 2. Understand Hella Before Changing Anything

Before implementation, inspect the Hella repository and understand the current architecture.

Read:

* compiler source
* parser
* AST/type system
* semantic analysis
* module/import resolution
* name resolution
* code generation
* runtime
* standard library
* build system
* test suite
* examples
* documentation
* existing compiler diagnostics

Understand how Hella currently represents:

* modules
* imports
* namespaces
* classes
* structs
* traits
* generics
* arrays
* vectors
* maps
* slices
* strings
* pointers
* nullable values
* `Result`
* errors
* enums/tagged unions
* methods
* constructors
* operators
* attributes
* FFI
* platform-specific code

Do not redesign unrelated language features merely because you personally prefer a different model.

The standard library should fit Hella.

---

# 3. Foundational Type Audit

Before redesigning individual modules, perform a dedicated audit of Hella's fundamental data types:

```text
string
str
arr
vec
map
```

These are not ordinary convenience libraries.

They are among the types users will interact with constantly.

Evaluate their:

* representation
* ownership
* mutability
* copying behavior
* allocation behavior
* indexing
* slicing
* iteration
* construction
* conversion
* comparison
* searching
* mutation
* formatting
* parsing
* error handling
* memory safety
* performance
* generic behavior
* interoperability
* platform independence
* compiler integration

If the current implementation is fundamentally insufficient, **fix the language/compiler/runtime**, rather than trying to cover structural problems with dozens of awkward helper functions.

The resulting APIs should feel like first-class Hella facilities.

---

# 4. Strings Are a Major Priority

Strings are one of the most important parts of Hella.

Do not treat the string API as "a few helpers around a character array".

Hella needs a **proper string system**.

Study the string systems of Rust, Odin, Go, and other relevant systems languages and determine what concepts are genuinely useful for Hella.

The final design must provide a clear distinction between:

```text
string
str
```

without introducing confusing or unsafe semantics.

---

## 4.1 `string`

`string` should be the primary general-purpose Hella string type.

It must be useful for ordinary application programming.

It should have comprehensive support for operations such as:

### Construction

* empty strings
* literals
* conversion from `str`
* conversion from characters
* conversion from byte sequences where valid
* formatted construction
* repeated strings where useful
* joining collections of strings

### Inspection

* length
* byte length where relevant
* character/code-point count where relevant
* empty checks
* searching
* prefix checks
* suffix checks
* containment
* equality
* comparison

Be explicit about whether a given operation works on:

```text
bytes
code points
```

or another representation.

Do not hide Unicode complexity behind misleadingly simple names.

### Slicing

Provide safe and useful slicing.

Consider:

```text
substring
slice
take
take_front
take_back
drop
drop_front
drop_back
```

or appropriate Hella-native equivalents.

Slicing must not accidentally split UTF-8 in an invalid way unless the API explicitly operates at the byte level.

### Searching

Support useful operations such as:

* find
* find last
* contains
* starts with
* ends with
* count
* replace
* replace all
* searching by character
* searching by substring

### Splitting

Provide:

* split
* split once
* split lines
* whitespace splitting
* delimiter-based splitting
* joining

The returned collection type should make sense in Hella.

### Trimming

Support:

* trim
* trim left
* trim right
* whitespace trimming
* custom-character trimming where useful

### Case conversion

Evaluate support for:

* lower case
* upper case
* case-insensitive comparison
* title/capitalization behavior where appropriate

Be conscious of Unicode semantics.

Do not implement ASCII-only behavior and label it as general Unicode support.

### Parsing

Support conversion such as:

```text
string -> int
string -> uint
string -> float
string -> double
string -> bool
```

and other useful primitive parsing.

Parsing should use Hella's error model.

Do not silently turn malformed input into a default value unless the API explicitly promises that behavior.

### Formatting

Strings must integrate with the global formatting system.

Support:

* interpolation
* formatting
* escaping
* quoting where appropriate
* debug/display representations

### Iteration

Provide useful iteration over:

* bytes
* Unicode code points / characters
* grapheme-aware functionality only if realistically justified

Do not pretend bytes and Unicode characters are interchangeable.

### Conversion

Provide clear conversion to and from:

```text
str
bytes
char
```

and other relevant types.

---

# 5. `str` Must Be a Real, Useful Type

Hella should have a distinct `str` type for **basic immutable strings**.

Do not simply introduce the name `str` and leave it as an alias for `string`.

Research how Rust, Odin, and other languages distinguish owned strings, string views/slices, immutable strings, literals, and byte-oriented data.

Then design the Hella version around Hella's own memory model.

The critical requirement is:

> `str` must be genuinely useful and semantically distinct from `string`.

Evaluate the appropriate semantics for Hella.

Depending on what Hella's ownership/reference-counting model can safely support, `str` may be:

* an immutable string representation,
* an immutable string view,
* a lightweight string slice,
* or another safe immutable representation.

However:

**Do not blindly reproduce Rust's `str` semantics.**

Rust's `str` is tightly coupled to Rust's ownership and lifetime system. Hella does not have Rust's lifetime model.

Therefore the Hella implementation must be safe and ergonomic **within Hella's own memory model**.

Do not introduce lifetime complexity merely to imitate Rust.

The compiler/runtime must guarantee that a `str` cannot become a dangling reference/view.

The user experience should be simple.

---

## 5.1 String Literals

Determine whether string literals should naturally have the `str` type, `string` type, or another compiler representation.

Choose the design that gives Hella:

* safe literals
* cheap literals
* clean APIs
* easy conversion
* predictable ownership
* no unnecessary allocation

The choice must integrate naturally with:

```hella
println("Hello")
```

and:

```hella
string name = "Hello"
```

---

# 6. String Performance and Memory

String operations must not be casually implemented through repeated allocations.

Audit:

* concatenation
* interpolation
* conversion
* slicing
* splitting
* replacement
* formatting
* builders/buffers

Provide an appropriate mutable string builder/buffer facility where useful.

For example, investigate whether Hella needs something conceptually equivalent to:

```text
StringBuilder
StringBuffer
Builder
```

but use a Hella-appropriate design and naming.

Common operations should not require users to manually manage allocation.

At the same time, avoid hidden O(n²) behavior in obvious operations such as repeated concatenation.

---

# 7. Unicode and Text Correctness

Treat UTF-8 and Unicode as serious concerns.

Determine the actual representation of Hella strings and explicitly document whether operations are:

* byte-based
* code-point-based
* Unicode-aware
* locale-aware

Do not make false promises.

At minimum, provide a clean distinction between byte-oriented operations and text-oriented operations.

UTF-8 invalid data should be handled intentionally.

---

# 8. Arrays

Hella already has arrays.

They need a **complete and coherent API**.

Audit both:

```text
TYPE arr name = [...]
```

and:

```text
TYPE arr[SIZE] name
```

as applicable to the current language.

Cover:

* length
* size
* indexing
* safe access
* iteration
* slicing where supported
* copying
* filling
* initialization
* clearing where meaningful
* searching
* contains
* count
* min/max where meaningful
* equality
* comparison where meaningful
* sorting where appropriate
* reversing
* swapping
* concatenation where appropriate
* conversion to vectors
* conversion to slices
* formatting/debugging

Make sure fixed-size arrays have semantics that actually distinguish them from vectors.

Do not create 20 redundant functions if one generic operation handles the concept cleanly.

---

# 9. Vectors

Hella's `vec` type must be a serious dynamic collection, not merely "an array that can grow".

Study Rust's `Vec`, Odin dynamic arrays, Go slices, and equivalent facilities.

Evaluate and implement useful operations such as:

### Construction

* empty vector
* literal construction
* capacity-based construction
* repeated values
* conversion from arrays
* conversion from slices
* conversion from iterables/appropriate Hella sources

### Capacity

* length
* capacity
* reserve
* reserve additional
* shrink
* clear

### Mutation

* push
* pop
* append
* prepend where justified
* insert
* remove
* remove at
* swap remove
* retain/filter
* truncate
* extend
* replace

### Access

* indexing
* first
* last
* safe access
* mutable access where Hella permits it
* slicing
* split operations where useful

### Searching

* contains
* find
* find index
* count
* equality
* predicate-based searching where appropriate

### Ordering

* sort
* stable sort where useful
* reverse
* sort by comparison/key where appropriate

Do not introduce a trait system solely to support sorting.

Use Hella's existing operator/comparison/generic mechanisms.

### Conversion

Allow sensible conversion between:

```text
arr
vec
slice
```

without pointless copying when a safe zero-copy representation is possible.

Document when conversion allocates.

---

# 10. Vector Iteration and Functional Operations

Because vectors will be heavily used, evaluate useful traversal operations.

Depending on Hella's existing language mechanisms, consider:

* iteration
* map/transform
* filter
* fold/reduce
* any
* all
* find
* position
* count
* take
* skip
* zip

However, do **not** blindly reproduce Rust's iterator framework or functional programming model.

These operations must fit Hella's object-oriented and imperative design.

Prefer straightforward Hella APIs over a huge abstraction hierarchy.

---

# 11. Maps

Hella has maps using syntax such as:

```hella
string:int ages = has
    "John": 25
    "Jane": 30
end
```

and:

```hella
string:bool admins = has
    "John": false
    "Jane": true
end
```

There is intentionally no `map` keyword.

Preserve this language design unless implementation evidence shows a fundamental problem.

Maps need to become a **complete standard collection**.

---

## 11.1 Map construction

Support:

* empty maps
* inline construction
* capacity-based construction where useful
* construction from other map/collection sources
* copying
* conversion

---

## 11.2 Map access

Provide clean operations for:

* get
* contains / has key
* indexing
* safe lookup
* insertion
* update
* remove
* clear
* get-or-default
* get-or-insert
* get-or-create where appropriate

Be very deliberate about the semantics of:

```hella
map[key]
```

Does missing data:

* return a nullable value?
* return a `Result`?
* insert a default?
* panic?

Choose one coherent model.

Do not let different collection APIs behave inconsistently.

---

## 11.3 Map iteration

Maps must provide useful iteration over:

* keys
* values
* entries

For example, concepts equivalent to:

```text
keys()
values()
entries()
```

should be evaluated.

Provide an ergonomic way to iterate key/value pairs.

Do not make users manually perform repeated hash lookups just to iterate.

---

## 11.4 Map utilities

Evaluate:

* count
* capacity
* reserve
* clear
* merge
* clone/copy
* equality where meaningful
* transformation
* filtering
* lookup helpers

Do not add operations that have poor semantics for an unordered map.

---

# 12. Collection Semantics Must Be Consistent

`arr`, `vec`, and maps should feel like parts of the same collection ecosystem.

Establish consistent conventions for:

* length naming
* access naming
* removal
* searching
* iteration
* copying
* conversion
* formatting
* equality
* error behavior

For example, do not have:

```text
vec.length()
arr.len()
map.count()
```

unless there is a strong semantic reason.

Likewise, do not use different names for equivalent concepts without justification.

Create a consistent Hella collection vocabulary.

---

# 13. Collection Memory Semantics

Audit all collection behavior against Hella's memory model.

Pay particular attention to:

* reference counting
* `.copy`
* nested collections
* mutation through references
* copying collections
* copying collection elements
* capacity growth
* ownership of contained objects
* deallocation
* cycles where applicable

For example, determine exactly what this means:

```hella
string vec names = ["Alice", "Bob"]
```

and:

```hella
string vec other = names.copy
```

Ensure the semantics are intentional and documented.

Do not accidentally create shallow-copy behavior where Hella expects independent data.

---

# 14. Core

Fundamental utilities used throughout Hella programs.

Examples:

* basic conversions
* equality/comparison helpers
* min/max
* clamp
* assertions
* panic/abort facilities where appropriate
* type utilities
* common constants
* memory-related helpers where safe and useful

Do not duplicate compiler primitives unnecessarily.

---

# 15. Bytes

Provide a first-class byte-oriented API for:

* buffers
* byte slices
* encoding/decoding
* reading/writing binary data
* efficient concatenation
* searching
* conversion to/from strings

This is especially important for files, networking, HTTP, serialization, and FFI.

---

# 16. Math

Provide useful mathematical functionality, including appropriate support for:

* integer operations
* floating-point operations
* powers
* roots
* logarithms
* exponentials
* trigonometry
* rounding
* absolute values
* signs
* interpolation where appropriate
* constants
* numeric predicates
* bit manipulation
* overflow-aware operations where appropriate

Account for Hella's integer types and floating-point types.

Make integer and floating-point APIs consistent.

---

# 17. Random

Provide a proper random subsystem.

Consider:

* default RNG
* deterministic seeded RNG
* integer ranges
* floating ranges
* booleans
* bytes
* shuffling
* sampling where useful

Separate:

```text
general-purpose pseudo-randomness
```

from:

```text
cryptographically secure randomness
```

Do not imply that a normal PRNG is cryptographically secure.

Use platform facilities for secure randomness.

---

# 18. I/O

Build a useful `std::io`.

It should cover concepts such as:

* readers
* writers
* buffered readers
* buffered writers
* stdin
* stdout
* stderr
* reading lines
* reading all data
* copying streams
* formatted output
* flushing
* byte operations

The API should compose with files, sockets, pipes, and other streams.

---

# 19. Filesystem

Create a serious filesystem API.

Cover:

* paths
* current directory
* absolute/canonical paths
* existence
* file metadata
* permissions where portable
* create/remove/rename
* copy/move
* files
* directories
* directory iteration
* temporary files/directories
* timestamps
* file size
* symlinks where supported
* path joining
* extensions
* file names
* parent paths
* normalization

Do not represent paths as ordinary string manipulation alone.

Use a dedicated path abstraction if that fits Hella.

Handle Windows path semantics correctly.

Examples that must be considered:

```text
C:\Users\...
C:/Users/...
\\server\share
/Users/...
/home/...
```

Do not assume `/` is always the filesystem separator.

---

# 20. OS

Create an OS/system API for common operating-system functionality.

Evaluate APIs for:

* environment variables
* process arguments
* process IDs
* parent process
* current user
* hostname
* platform information
* executable path
* home directory
* temporary directory
* process execution
* exit codes
* signals where portable
* process spawning
* pipes
* environment inheritance
* working directory
* system information

Separate portable APIs from explicitly platform-specific facilities.

---

# 21. Process Management

Provide a useful process API.

It should support concepts such as:

```text
spawn
wait
stdin
stdout
stderr
environment
working directory
exit status
```

Where possible, expose both:

```text
simple convenience APIs
```

and:

```text
structured process APIs
```

Do not make shell invocation the only process abstraction.

Avoid APIs that silently invoke a shell when direct process execution is possible.

---

# 22. Terminal

Hella needs a proper terminal library.

At minimum evaluate:

* stdout/stderr
* terminal size
* ANSI styles
* colors
* cursor positioning
* clearing
* cursor visibility
* terminal detection
* interactive input
* raw/cooked modes where supported
* Windows console support
* Unicode considerations

The terminal API must work on:

```text
Windows
Linux
macOS
```

Do not assume ANSI behavior is identical across all platforms.

---

# 23. Formatting

Build a strong formatting system.

It should support:

* strings
* integers
* unsigned integers
* floats
* booleans
* chars
* collections
* user-defined types
* alignment
* width
* precision
* numeric bases
* escaping
* debug representations
* display representations where the type system supports that distinction

Collections and strings must integrate naturally with formatting.

For example, formatting a vector, array, map, or string should produce useful output without the user having to write custom conversion code.

Make this integrate naturally with:

```text
println
print
log
errors
terminal output
```

Do not build a completely separate formatting mechanism for every subsystem.

---

# 24. Logging

Provide a practical logging subsystem.

Consider:

* levels
* timestamps
* categories/targets
* structured fields where appropriate
* stdout/stderr routing
* configurable output
* formatting
* filtering
* debug/development logging

At minimum support:

```text
debug
info
warn
error
```

Keep the basic API simple.

---

# 25. Time and Dates

Provide useful time primitives.

Separate:

```text
instant / monotonic time
```

from:

```text
wall-clock date/time
```

Support:

* timestamps
* durations
* sleeping
* monotonic timers
* elapsed time
* UTC
* local time
* date components
* formatting/parsing
* comparisons
* arithmetic

Do not use wall-clock time for duration measurement.

---

# 26. Networking

Build a useful networking layer.

Evaluate support for:

* IP addresses
* sockets
* TCP
* UDP
* DNS
* listeners
* connections
* timeouts
* local/remote addresses
* shutdown
* buffers
* IPv4
* IPv6

Keep low-level networking available without forcing every user into a low-level socket API.

---

# 27. HTTP

Hella should have a real HTTP library.

Cover at minimum:

* HTTP methods
* requests
* responses
* headers
* status codes
* URLs
* bodies
* query parameters
* request building
* response reading
* redirects
* timeouts
* connection reuse where appropriate
* HTTP client
* HTTP server
* routing primitives where appropriate

Design a clear separation between:

```text
network primitives
HTTP primitives
HTTP convenience utilities
```

Do not turn the HTTP library into a web framework.

Investigate TLS support carefully.

---

# 28. URL

Provide proper URL handling.

Support:

* parsing
* validation
* scheme
* host
* port
* path
* query
* fragment
* escaping
* decoding
* building URLs

Do not treat URLs as arbitrary strings.

---

# 29. Serialization / Encoding

Evaluate standard APIs for:

* JSON
* UTF-8
* hexadecimal
* Base64
* binary encoding
* textual encoding helpers

JSON should be practical enough for:

* configuration
* APIs
* network requests
* serialization of Hella types where sensible

Do not invent an unnecessarily complex reflection framework just to support serialization.

---

# 30. Hashing / Checksums / Crypto

Evaluate useful cryptographic and integrity primitives.

Consider:

* generic hashing interfaces
* common hashes
* checksums
* cryptographically secure randomness
* cryptographic primitives that have a strong justification for being standard

Be careful not to invent custom cryptography.

---

# 31. Concurrency and Synchronization

Inspect Hella's actual runtime capabilities before committing to the API.

Where the language/runtime supports it cleanly, evaluate:

* threads
* mutexes
* read/write locks
* atomics
* condition variables
* channels
* wait groups
* thread-local storage
* synchronization primitives

Do not build a fake concurrency abstraction that cannot be correctly implemented.

---

# 32. Reflection / Compile-Time Facilities

Hella already has compile-time metadata/processing facilities.

Use those capabilities where they provide real value.

Investigate whether they can support:

* serialization
* formatting
* diagnostics
* code generation
* type inspection
* metadata-driven utilities

Prefer compile-time solutions where possible.

---

# 33. FFI and Platform APIs

The standard library may need compiler/runtime support for platform integration.

Use Hella's existing FFI architecture rather than inventing multiple unrelated mechanisms.

For operating-system functionality, isolate:

```text
portable API
platform backend
```

Example conceptually:

```text
std::fs
    ├── common implementation
    ├── windows backend
    ├── linux backend
    └── macos backend
```

Do not leak platform-specific types into portable APIs unless unavoidable.

---

# 34. The Import / Namespace Problem

This is mandatory.

Currently:

```hella
import std::io

void main() do
    println("Hello")
end
```

works.

However:

```hella
import std::io

void main() do
    io::println("Hello")
end
```

does not.

Fix this.

## Required semantics

An imported module must have a usable module namespace.

For:

```hella
import std::io
```

the imported module should be addressable as:

```hella
io::println(...)
io::read(...)
io::write(...)
```

while preserving the currently supported direct-import behavior:

```hella
println(...)
read(...)
write(...)
```

unless there is a compelling language-architecture reason that makes this impossible.

The point is to support both convenience and explicit disambiguation.

For example:

```hella
import std::io
import std::log

void main() do
    io::println("Hello")
    log::println("Starting program")
end
```

If two imported modules expose the same symbol, an unqualified use should become ambiguous rather than silently selecting one.

Implement this properly in the compiler's module/name-resolution system.

Do not implement a fragile textual substitution.

---

# 35. Import Resolution Requirements

Review the entire import architecture.

Determine how Hella currently resolves:

```hella
import std::io
import std::io::{println, read}
```

and related forms.

Add proper namespace handling without creating:

* duplicate symbol tables
* import-order dependence
* accidental shadowing
* circular-resolution bugs
* linker collisions
* code-generation duplication

The namespace should refer to the actual imported module.

Do not generate duplicate functions merely because both:

```hella
println()
```

and:

```hella
io::println()
```

are used.

Both should refer to the same declaration.

---

# 36. Compiler Changes Are Allowed

You may modify the compiler, runtime, build system, or language implementation where necessary.

However:

> **Do not change language semantics casually.**

Before changing compiler behavior:

1. Understand the existing implementation.
2. Identify the concrete standard-library requirement.
3. Determine whether the problem can be solved inside the library.
4. If not, make the smallest coherent compiler/runtime change.
5. Add regression tests.
6. Verify existing behavior has not been accidentally broken.

Compiler changes should support the language rather than turning this task into an unrelated language redesign.

If `string`, `str`, `arr`, `vec`, or `map` require compiler/runtime support to be implemented correctly, that is explicitly within scope.

---

# 37. Portability Strategy

Every OS-facing feature must explicitly answer:

```text
Windows?
Linux?
macOS?
```

For every platform:

* identify available system APIs
* identify semantic differences
* determine what can share an implementation
* determine what requires a backend

Build portability into the architecture rather than adding it after implementation.

---

# 38. API Review Against Rust, Odin, and Go

For each major module, compare Hella's design against the actual installed libraries.

Do not ask:

> "How can Hella copy Rust?"

Ask:

> "What problem is Rust solving here?"

> "What problem is Odin solving here?"

> "What problem is Go solving here?"

Then ask:

> "What is the cleanest Hella solution?"

Study particularly:

### Rust

Look at the design of:

* `std::io`
* `std::fs`
* `std::path`
* `std::env`
* `std::process`
* `std::net`
* `std::time`
* `std::thread`
* `std::sync`
* `std::collections`
* `std::fmt`
* `std::num`
* `std::str`
* `std::string`
* `std::ffi`

Pay special attention to Rust's distinctions between:

* `String`
* `&str`
* byte strings
* byte slices
* arrays
* slices
* vectors
* hash maps
* ordered maps
* sets
* iterators

Use those concepts as research material while adapting them to Hella's memory model.

### Go

Study packages such as:

* `fmt`
* `io`
* `os`
* `path`
* `path/filepath`
* `strings`
* `bytes`
* `bufio`
* `net`
* `net/http`
* `encoding/*`
* `time`
* `math`
* `math/rand`
* `crypto/*`
* `sync`
* `os/exec`
* `log`

Pay attention to Go's simplicity and package-level decomposition.

Also study Go's:

* string semantics
* byte slices
* rune handling
* slices
* maps
* formatting
* I/O interfaces

### Odin

Study its actual:

* core packages
* strings
* dynamic arrays
* slices
* maps
* collections
* filesystem APIs
* OS APIs
* terminal APIs
* formatting
* random facilities
* networking
* threading/synchronization
* platform handling

Pay particular attention to where Odin chooses simple low-level APIs instead of elaborate abstractions.

---

# 39. Do Not Copy Unnecessary Abstractions

Avoid automatically adding concepts such as:

* traits/interfaces solely because Rust or Go has them
* iterator frameworks that fight Hella's object model
* lifetime machinery
* async runtimes
* reflection systems
* dependency injection
* elaborate error hierarchies
* generic frameworks
* enterprise-style networking abstractions

Every abstraction must have a reason in Hella.

---

# 40. Standard Library Organization

Establish a coherent module tree.

A possible direction is:

```text
std::
    core
    string
    bytes
    collections
    io
    fmt
    fs
    path
    os
    process
    env
    time
    math
    rand
    term
    log
    net
    http
    url
    json
    encoding
    hash
    crypto
    sync
    thread
```

This is a starting point, not a mandatory final structure.

Do not create a separate module for every tiny collection/string helper merely for the sake of organization.

The user experience should remain discoverable.

---

# 41. Public API Quality

Every public API should pass these questions:

1. Is the name obvious?
2. Is the behavior obvious?
3. Does it match nearby APIs?
4. Is it portable?
5. Does it return the right Hella type?
6. Is failure represented correctly?
7. Is the API composable?
8. Does it avoid unnecessary allocation?
9. Is the simple case simple?
10. Does the advanced case remain possible?
11. Is the API testable?
12. Is it documented?

Additionally for collections and strings:

13. Does the API behave consistently with the other collection/string types?
14. Is allocation behavior understandable?
15. Are byte/text semantics explicit?
16. Is copying behavior consistent with Hella's `.copy` semantics?
17. Is iteration ergonomic?
18. Are common operations discoverable?

---

# 42. Performance

Do not sacrifice performance unnecessarily.

The standard library should avoid:

* hidden allocations in trivial operations
* unnecessary copies
* repeated conversions
* platform abstraction overhead in hot paths
* unnecessary heap allocation
* needless synchronization
* O(n²) behavior hidden inside common string/collection operations

At the same time, do not destroy API ergonomics for micro-optimizations.

Measure important operations rather than guessing.

Benchmark:

* string concatenation
* string searching
* string splitting
* vector growth
* map insertion/lookup
* collection copying
* formatting
* filesystem operations where meaningful

---

# 43. Memory and Ownership

Hella's memory model must remain authoritative.

Do not import Rust's ownership semantics into Hella merely because they are familiar.

When designing collections, strings, buffers, file handles, sockets, and other resources:

* understand Hella reference counting
* understand `.copy`
* understand value/reference semantics
* ensure resources are released correctly
* use constructors/destructors appropriately
* avoid accidental shallow copies where Hella does not intend them
* ensure string views such as `str`, if implemented as views, can never dangle

Resource-owning objects should have clear lifetimes.

---

# 44. Documentation

Every substantial module must receive documentation.

Document:

* purpose
* import path
* major types
* common operations
* failure behavior
* platform caveats
* examples
* common patterns
* allocation/copy behavior where relevant

Strings and collections deserve particularly strong documentation.

For each of:

```text
string
str
arr
vec
map
```

document:

* what it represents
* ownership
* mutability
* copying
* allocation
* indexing
* slicing
* iteration
* conversion
* common methods
* performance considerations

Examples should be real Hella code.

Do not document APIs that do not actually compile.

---

# 45. Tests

Add tests for every subsystem.

## Fundamental type tests

Create extensive tests for:

```text
string
str
arr
vec
map
```

Test:

* construction
* empty values
* indexing
* mutation
* copying
* equality
* searching
* slicing
* iteration
* conversion
* formatting
* nesting
* nullable values where relevant
* allocation-sensitive operations where practical
* error handling
* Unicode
* invalid UTF-8 handling
* capacity growth
* map collision behavior where practical
* nested collection ownership/reference behavior

## Unit tests

Also test:

* string handling
* formatting
* parsing
* collections
* math
* random
* filesystem
* paths
* time
* networking
* HTTP
* JSON
* terminal utilities
* process utilities
* synchronization

## Compiler tests

For:

* module imports
* namespace-qualified imports
* ambiguous imports
* selective imports
* type resolution
* `string`
* `str`
* arrays
* vectors
* maps
* compiler diagnostics
* cross-module resolution

## Integration tests

For real programs using the standard library.

## Platform tests

Where possible:

```text
Windows
Linux
macOS
```

Do not pretend portability exists just because the code compiles on one platform.

---

# 46. Required Import Tests

These cases must work after the compiler changes.

## Existing direct usage

```hella
import std::io

void main() do
    println("Hello")
end
```

## Qualified usage

```hella
import std::io

void main() do
    io::println("Hello")
end
```

## Both forms

```hella
import std::io

void main() do
    println("Hello")
    io::println("Hello again")
end
```

## Multiple modules

Where two modules expose the same symbol:

```hella
import foo::x
import bar::x
```

an unqualified:

```hella
something(...)
```

must produce a clear ambiguity diagnostic when appropriate.

The explicitly qualified version must continue to work.

---

# 47. Compatibility Rule

Do not arbitrarily break existing Hella programs.

When replacing an old API:

Prefer, when practical:

```text
new canonical API
+
temporary compatibility alias
```

rather than instantly deleting the old API.

However, if an existing API is fundamentally harmful or makes the new design incoherent, remove it deliberately and update the repository accordingly.

Document deliberate breaking changes.

---

# 48. Implementation Workflow

Work in phases.

## Phase 1 — Audit

Inspect:

* Hella compiler
* Hella stdlib
* Rust std
* Odin core libraries
* Go stdlib

Produce an internal module/API map.

Also produce dedicated audits for:

```text
string
str
arr
vec
map
```

---

## Phase 2 — Architecture

Define:

* module tree
* naming conventions
* error conventions
* ownership/resource conventions
* collection conventions
* string semantics
* `string` vs `str`
* portability architecture
* namespace/import behavior

Do not start mass implementation before this is coherent.

---

## Phase 3 — Compiler Foundations

Implement the minimum compiler/runtime changes required for:

* proper module namespaces
* qualified imports
* ambiguity detection
* robust collection/string primitives
* platform-aware library organization
* any other necessary stdlib infrastructure

Add compiler tests before building heavily on the new behavior.

---

## Phase 4 — Foundational Types

Prioritize:

```text
string
str
arr
vec
map
bytes
```

Make these genuinely complete before relying on them throughout the rest of the standard library.

---

## Phase 5 — Foundation Libraries

Implement:

```text
core
fmt
io
fs
path
env
os
process
```

These become the foundation for everything else.

---

## Phase 6 — Systems Libraries

Implement:

```text
time
math
rand
term
log
net
http
url
sync
thread
```

where supported by the current runtime.

---

## Phase 7 — Data / Encoding Libraries

Implement:

```text
json
encoding
hash
crypto
```

as justified by the architecture.

---

## Phase 8 — Integration

Update:

* examples
* compiler tests
* documentation
* sample programs
* existing Hella projects inside the repository

Replace outdated stdlib usage.

---

# 49. Do Not Stop at Compilation

A library is not "done" merely because:

```text
compiler succeeds
```

Check real usability.

Write small programs that exercise the APIs.

### CLI program

Reads arguments, parses strings, formats output, writes files, and logs information.

### File utility

Walks directories and manipulates files.

### Text-processing utility

Reads text, searches/splits/replaces strings, uses vectors and maps, and emits formatted output.

### HTTP client

Makes a request and parses JSON.

### HTTP server

Accepts requests and writes responses.

### Networking program

Uses TCP/UDP.

### Terminal application

Uses formatting, terminal control, and user input.

### Data-processing program

Uses strings, arrays, vectors, maps, parsing, and serialization.

### Process wrapper

Spawns a child process and captures output.

### Collection-heavy program

Uses nested arrays, vectors, maps, string keys, iteration, sorting, searching, copying, and mutation.

The standard library should make these programs pleasant to write.

---

# 50. Quality Gate

Before declaring the overhaul complete, verify:

```text
[ ] stdlib audited
[ ] Rust inspected from local installation
[ ] Odin inspected from local installation
[ ] Go inspected from local installation

[ ] string redesigned/audited
[ ] str exists with clearly distinct semantics
[ ] string and str integrate cleanly
[ ] UTF-8/text behavior is explicit
[ ] byte-oriented string operations exist
[ ] string parsing exists
[ ] string formatting exists
[ ] string searching exists
[ ] string splitting exists
[ ] string replacement exists
[ ] string trimming exists
[ ] string iteration exists
[ ] string conversion APIs exist
[ ] string allocation/copy behavior is documented

[ ] arrays fully audited
[ ] arrays have useful indexing/access APIs
[ ] arrays have useful search/iteration APIs
[ ] arrays have useful slicing/conversion APIs

[ ] vectors fully audited
[ ] vectors support growth/capacity management
[ ] vectors support insertion/removal
[ ] vectors support searching
[ ] vectors support sorting/reversing
[ ] vectors support iteration
[ ] vectors support useful conversion APIs

[ ] maps fully audited
[ ] maps support safe lookup
[ ] maps support insertion/update/removal
[ ] maps support keys/values/entries
[ ] maps support iteration
[ ] maps have coherent missing-key semantics
[ ] map copying/reference behavior is correct

[ ] collection conventions are consistent
[ ] string/collection formatting works
[ ] import namespaces implemented
[ ] qualified module access works
[ ] ambiguous unqualified symbols are diagnosed
[ ] existing direct imports remain usable where intended

[ ] filesystem is cross-platform
[ ] paths are platform-aware
[ ] process APIs are useful
[ ] environment APIs exist
[ ] terminal support exists
[ ] formatting is coherent
[ ] logging exists
[ ] time API exists
[ ] math API exists
[ ] random API exists
[ ] I/O is composable
[ ] networking is useful
[ ] HTTP is useful
[ ] URL support exists
[ ] JSON/encoding support exists
[ ] synchronization exists where runtime permits

[ ] compiler changes have regression tests
[ ] platform-specific code is isolated
[ ] documentation exists
[ ] examples compile
[ ] tests pass
[ ] no unrelated language behavior was broken
```

---

# 51. Final Rule

When faced with a choice between:

```text
"this is how another language does it"
```

and:

```text
"this is the cleanest solution for Hella"
```

choose the second.

Rust, Odin, and Go are **research material**.

They are not Hella's design specification.

The final result should feel like:

> "This is what Hella's standard library should obviously look like."

not:

> "This is Rust's stdlib with Hella syntax."

Above all, prioritize **usefulness**.

A small, excellent API is better than a giant collection of technically impressive but rarely useful functions.

However, "small" must not be used as an excuse for an incomplete standard library.

When a type is fundamental to everyday programming — especially:

```text
string
str
arr
vec
map
```

— provide the full set of functionality a programmer would reasonably expect.

The finished Hella standard library should make ordinary systems programming, CLI development, file manipulation, text processing, terminal applications, networking, HTTP work, data processing, and general utilities straightforward without requiring users to immediately reach for third-party dependencies.

