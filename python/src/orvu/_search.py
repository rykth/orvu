"""Search by meaning: what a search finds."""

import dataclasses


@dataclasses.dataclass(frozen=True)
class Found:
    """A note, a part of a document or a task that a search found.

    `kind` is "note" for a simple note, "structured" for a structured note,
    "document" for a paragraph of a document and "task" for a task. `name` is
    the note's or the document's name, or the task's id. `paragraph` is the
    paragraph's number in the document, counted from 1, and `title` the
    task's title; each is `None` for the others. `text` is what was searched:
    for a note, its name and what it says, and for a task, its title and its
    notes. `score` says how close its meaning is to the question's, from 0
    to 1.
    """

    kind: str
    name: str
    text: str
    score: float
    paragraph: int = None
    title: str = None

    @classmethod
    def _of(cls, found):
        return cls(
            found["kind"], found["name"], found["text"], found["score"], found.get("paragraph"), found.get("title")
        )
