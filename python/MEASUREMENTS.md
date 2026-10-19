# How fast Orvu is, and how much room it takes

Numbers from one run on one computer, and how they were got, so that you can
get your own.

```text
python -m orvu.measure
```

It takes a minute or two and prints the tables below for your computer.

## How it is measured

- **Three machines on one computer.** Three sidecars are set up with
  `orvud setup`, as real machines are, and reach each other over the real
  encrypted link. Only the network is missing.
- **Through the Python library.** Every number is what a Python program that
  uses Orvu would see, the library's own time included.
- **One thing at a time, unless it says otherwise.** For "each second", a
  program does a thing a few thousand times and the time is divided. For a
  time in thousandths of a second, the thing is done several times and the
  middle time is taken.
- **"Several programs" are programs.** Each is started by itself, connects
  by itself, and all are let go at the same moment.
- **The program that measures is one file,** `orvu/measure.py`, and is meant
  to be read.

<!-- measured: start -->
## The numbers

- **Computer:** 12th Gen Intel(R) Core(TM) i7-1255U, 12 cores, Linux 7.2.3-arch1-3, Python 3.14.7
- **Orvu:** 0.1.0, sidecar `.../x86_64-unknown-linux-gnu/release/orvud`
- **How:** `python -m orvu.measure`, three machines on this one computer

### One program, one thing after another

| What | Each second |
|---|---|
| Write a note | 4,483 |
| Read a note | 11,069 |
| Spend from a budget | 2,992 |
| Write a note, 100 in one message | 33,902 |
| Add, take and finish a task | 331 |

### Several programs on one machine, each writing its own note over and over

| Programs | Writes each second, all together | Usual wait for one write (ms) | Slowest 1 in 100 (ms) |
|---|---|---|---|
| 1 | 5,432 | 0.15 | 0.45 |
| 4 | 8,768 | 0.34 | 2.01 |
| 16 | 7,268 | 1.70 | 5.57 |
| 64 | 6,937 | 8.06 | 20.19 |

### From one machine to another

| What | Usual (ms) | Slowest 1 in 20 (ms) | Slowest (ms) |
|---|---|---|---|
| A note written on alpha can be read on beta | 8.70 | 9.30 | 107.28 |

### As the notes of a team grow (one machine of three)

| Notes | On disk (MB) | Memory of the sidecar (MB) | Write one (ms) | Read one (ms) | Read them all (ms) |
|---|---|---|---|---|---|
| 1,000 | 8.5 | 140.1 | 0.37 | 0.26 | 0.95 |
| 5,000 | 8.5 | 140.4 | 0.38 | 0.22 | 4.32 |
| 10,000 | 8.5 | 144.0 | 0.32 | 0.18 | 8.72 |
| 20,000 | 16.6 | 164.3 | 0.31 | 0.20 | 18.03 |

### As the task board grows

| Tasks | Add one (ms) | Change one (ms) | List them all (ms) |
|---|---|---|---|
| 100 | 0.35 | 1.05 | 1.05 |
| 1,000 | 1.40 | 0.73 | 7.30 |
| 5,000 | 5.39 | 1.10 | 63.11 |

### Starting up

| What | Thousandths of a second |
|---|---|
| One sidecar, from nothing to answering | 7.97 |
<!-- measured: end -->

## What the numbers say

These sentences are about the run shown above.

- **One program does a few thousand small things a second.** Sending a
  hundred at once in a batch is several times faster for each.
- **A change reaches another machine in a few thousandths of a second.**
  Most of that is the sidecar's own doing: it gathers changes for up to ten
  thousandths of a second before it sends them. Now and then one takes
  longer: see the last column.
- **One sidecar does several thousand writes a second, however many
  programs send them.** A few programs get more done together than one.
  Past that the total does not grow, and each program waits longer: with
  sixty-four, a write takes about ten thousandths of a second. These totals
  move a good deal from one run to the next.
- **Reading or writing one note takes the same among twenty thousand as
  among a thousand:** well under a thousandth of a second. This is the
  number to know, because the simple notes of a team are one object.
  Reading all of them grows with their number, as it must.
- **Changing one task hardly depends on the size of the board:** about one
  thousandth of a second among a hundred tasks, a thousand or five
  thousand. Adding a task grows with the board, several times over from a
  thousand tasks to five thousand. Most of that is placing the new task in
  the tree of sub-tasks, which was timed by itself apart from this page.
  Listing all the tasks grows with their number, as it must: five thousand
  take a few hundredths of a second.
- **Memory stays level.** The sidecar held between a hundred and thirty and
  a hundred and seventy megabytes from a thousand notes to twenty thousand.
- **A sidecar starts in under a hundredth of a second.**

## What they do not say

- **Nothing about a network.** The three machines are one computer. Between
  real machines, add the time the network takes.
- **Nothing about large values.** Every note here is a short text.
- **Nothing about days.** Each run starts empty and lasts a minute. What
  weeks of use do to the room on disk was not measured.
- **Why the room on disk grows in steps** was not looked into. It stays the
  same over several thousand notes, and then doubles.
- **Why now and then a change takes far longer to cross than the rest** was
  not looked into either.
- **One computer, one run.** The numbers move from run to run, and a faster
  or slower computer gives others. Run it on yours.
- **The programs are Python.** Part of every time is Python's own. A program
  in another language would see other numbers.
