"""Documents: longer text that several agents write together."""

from . import _names
from ._handles import RichText
from ._proto import dataplane_pb2 as pb


class Document(RichText):
    """One shared text, such as a report or a plan. Get it from
    `team.document(name)`.

        report = team.document("q3-report")
        report.append("Sales rose 4%.\\n")
        report.text()   # "Sales rose 4%.\\n"

    Agents can insert and delete at any position at the same moment, and
    every edit is kept. The merge is mechanical: two agents that rewrite the
    same sentence get both versions tangled together.

    It works as an ordinary `orvu.RichText` does, with two things added.
    What an agent inserts is marked with its name, and the document is
    written down in the team's index the first time it is written to. The
    text and its mark go in together, so the mark lands on exactly that text
    even when another agent changes the same document at the same moment.
    """

    #: The mark that says which agent wrote a piece of the text.
    BY = "by"

    def __init__(self, team, name):
        self._short_name = "doc/" + _names.part(name, "the name of a document")
        super().__init__(team.client, _names.document(team.name, name))
        self._team = team

    def insert(self, index, text):
        """Inserts `text` at position `index`. Past the end means at the
        end. The text is marked as written by this agent, if it has a name."""
        return self._put(index, text)

    def append(self, text):
        """Adds `text` at the end."""
        return self._put(None, text)

    # Past the end of any text, which the engine takes to mean "at the end".
    _END = 2**31

    def _put(self, index, text):
        """Inserts `text` at `index`, or at the end if `index` is `None`."""
        if text == "":
            # Nothing to put in. It does not start a document either.
            return None
        self._team._record(self._short_name, "richtext")
        if self._team.me is None:
            return RichText.insert(self, self._END if index is None else index, text)
        # The text and its mark go in as one change, and the mark is tied to
        # the characters themselves, so another agent writing at the same
        # moment can neither take the mark nor leave this text without it.
        # Nothing is read first: the end is found where the change is made.
        message = pb.RichtextInsertMarked(
            object=self.name,
            index=self._END if index is None else index,
            text=text,
            attr=self.BY,
            value=self._team.me,
        )
        return self._run("richtext_insert_marked", message)

    def delete(self, index, length):
        self._team._record(self._short_name, "richtext")
        return RichText.delete(self, index, length)

    def format(self, index, length, attr, value):
        self._team._record(self._short_name, "richtext")
        return RichText.format(self, index, length, attr, value)

    def unformat(self, index, length, attr):
        self._team._record(self._short_name, "richtext")
        return RichText.unformat(self, index, length, attr)

    delete.__doc__ = RichText.delete.__doc__
    format.__doc__ = RichText.format.__doc__
    unformat.__doc__ = RichText.unformat.__doc__

    def written_by(self):
        """The text in pieces, each with the agent that wrote it:
        `[("Sales rose 4%.", "researcher-2"), (" Costs fell.", None)]`.
        `None` is text from a program that gave no name."""
        pieces = []
        for span in self.to_spans():
            who = span["marks"].get(self.BY)
            if pieces and pieces[-1][1] == who:
                pieces[-1] = (pieces[-1][0] + span["text"], who)
            else:
                pieces.append((span["text"], who))
        return pieces
