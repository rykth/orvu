# Merging in plain words

Every machine has its own copy of the data and changes it without asking
anyone. So two machines can change the same thing at the same moment. Orvu
needs a rule that turns any mix of changes into one answer, the same answer
on every machine, whatever order the changes arrive in. Data types built
with such a rule are called CRDTs, short for conflict-free replicated data
types.

## The idea

Think of a shared shopping list on two phones with no signal. One phone puts
milk on it, the other bread. When they meet, the list should hold both. A
CRDT is a list, a counter or a map that knows how to do that by itself:

- **Each change is a small record of what changed,** called a delta. It is
  sent to the other machines and merged into their copies.
- **Merging does not care about order or repeats.** A delta can arrive late,
  out of order, or twice, and the result is still the same.
- **Every type has a fixed rule for changes made at the same time,** so no
  machine ever has to ask which one wins.

That is why Orvu needs no central server and no locks, and why a machine that
was cut off can carry on and catch up later.

## The rules, type by type

| Kind of object | What it holds | When two machines change it at once |
|---|---|---|
| Counter | A number to add to and subtract from | Both changes count |
| Bounded counter | A number that never goes below zero, such as a budget | Each machine spends only its own share, so it is never overspent |
| Register | One value | The later write wins |
| Set | Unique items | An add beats a removal made at the same time |
| Map | Keys with values | Values merge; adding a key beats removing it |
| Multi-value map | Keys with text | Both writes are kept until a later write replaces them |
| List | Items in order | Both inserts are kept, in the same order everywhere |
| Rich text | Text with formatting | All typing is kept; the later formatting wins |
| JSON document | Nested fields and lists | Different fields merge; for the same field the later write wins |
| Tree | Items nested under items | A move that would make a loop is skipped |

The building blocks are made from these. A team's notes are a multi-value
map, which is how [a clash is kept](clashes.md). A budget is a bounded
counter. A document is rich text that also marks who wrote each part. The
task board is a JSON document, with a tree for sub-tasks.

## "Later" means by the clock

Where the later write wins, "later" is decided by a timestamp made from the
machine's clock, with the machine's number to break a tie. So keep the
machines' clocks right. A machine whose clock runs ahead wins more often than
it should, and a change stamped more than five minutes ahead of the
receiving machine's clock is refused.

"The later write wins" also means the earlier write is dropped without a
word. Where both values matter, use a type that keeps both, such as the notes.

## A budget is the one that says no

Most types never refuse a change. A bounded counter does: it never goes
below zero, on any machine, even when machines cannot reach each other. Each
machine holds a share of the budget, called its allowance, and spends only
from that. A machine that runs out asks a machine it can reach for more,
and the machines also move allowance towards the ones that spend most. A
machine that is cut off can only spend what it holds, so it may be told no
while there is budget elsewhere. It is never overspent.

## Where next

- [A clash is kept](clashes.md)
- [`crates/orvu-crdt`](../../crates/orvu-crdt) has every type, with examples
  and every rule spelled out.
