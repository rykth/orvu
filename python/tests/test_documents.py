"""Documents: longer text that several agents write together."""

import pytest

import orvu


def test_a_document_is_written_and_read(team):
    report = team.document("q3-report")
    assert report.text() == ""
    report.append("Sales rose 4%.")
    report.append(" Costs fell.")
    report.insert(0, "Q3: ")
    assert report.text() == "Q3: Sales rose 4%. Costs fell."
    report.delete(0, 4)
    assert team.document("q3-report").text() == "Sales rose 4%. Costs fell."


def test_what_an_agent_writes_is_marked_with_its_name(team, bob, sidecar):
    report = team.document("q3-report")
    report.append("Sales rose 4%.")
    bob.document("q3-report").append(" Costs fell.")
    report.insert(0, "Q3: ")
    assert report.written_by() == [("Q3: Sales rose 4%.", "alice"), (" Costs fell.", "bob")]

    # Text put in the middle of another agent's is the newcomer's.
    bob.document("q3-report").insert(4, "(draft) ")
    assert report.written_by() == [
        ("Q3: ", "alice"),
        ("(draft) ", "bob"),
        ("Sales rose 4%.", "alice"),
        (" Costs fell.", "bob"),
    ]
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        nameless.document("q3-report").append(" The end.")
    assert report.written_by()[-1] == (" The end.", None)
    assert report.text() == "Q3: (draft) Sales rose 4%. Costs fell. The end."


# Positions count characters, and some characters take two places in other
# languages. The mark has to land on exactly what was written.
def test_the_mark_lands_on_exactly_what_was_written(team, bob):
    page = team.document("notes")
    page.append("héllo 🙂 ")
    bob.document("notes").append("wörld 🌍")
    page.insert(999, "!")
    assert page.written_by() == [("héllo 🙂 ", "alice"), ("wörld 🌍", "bob"), ("!", "alice")]


def test_other_marks_work_as_on_plain_rich_text(team):
    page = team.document("notes")
    page.append("Sales rose 4%.")
    page.format(0, 5, "bold", "true")
    assert page.to_spans()[0] == {"text": "Sales", "marks": {"bold": "true", "by": "alice"}}
    page.unformat(0, 5, "bold")
    assert page.written_by() == [("Sales rose 4%.", "alice")]


def test_the_team_lists_its_documents_once_they_are_written_to(team, bob):
    team.document("empty").text()
    assert team.documents() == [] and team.index() == {}
    team.document("q3-report").append("x")
    bob.document("plan").append("y")
    assert team.documents() == ["plan", "q3-report"]
    assert team.index()["doc/plan"] == "richtext"


@pytest.mark.parametrize("name", ["", "a/b", "two words", None])
def test_a_document_needs_a_name_that_can_be_used(team, name):
    with pytest.raises(orvu.BadRequest, match="name of a document"):
        team.document(name)


def test_a_document_is_watched(team, bob):
    seen = []
    watching = team.document("q3-report").watch()
    bob.document("q3-report").append("Sales rose 4%.")
    for spans in watching:
        seen.append(spans)
        break
    assert "".join(span["text"] for span in seen[0]) == "Sales rose 4%."
    watching.close()


# Adding to the end is one message: the text and its mark go in together,
# and nothing is read first to find the end.
def test_appending_is_one_message(team):
    report = team.document("q3-report")
    report.append("Sales rose 4%.")
    sent = []
    real = team.client._call

    def call(send, **options):
        sent.append(1)
        return real(send, **options)

    team.client._call = call
    report.append(" Costs fell.")
    assert len(sent) == 1
    assert report.written_by() == [("Sales rose 4%. Costs fell.", "alice")]


class _Racing:
    """Stands in front of a client's stub, and has another agent write to
    the document just before this client's text goes out."""

    def __init__(self, stub, race):
        self._stub, self._race = stub, race

    def __getattr__(self, name):
        return getattr(self._stub, name)

    def _writes(self, requests):
        if any(r.WhichOneof("op") in ("richtext_insert", "richtext_insert_marked") for r in requests):
            self._race()

    def Execute(self, request, **options):
        self._writes([request])
        return self._stub.Execute(request, **options)

    def Batch(self, request, **options):
        self._writes(request.requests)
        return self._stub.Batch(request, **options)


# Another agent puts text in at the moment this one adds to the end. The new
# text still goes at the end, and the mark still lands on exactly it.
def test_the_mark_holds_when_another_agent_writes_at_the_same_moment(team, bob):
    report = team.document("q3-report")
    report.append("Sales rose 4%.")
    raced = []

    def race():
        if not raced:
            raced.append(1)
            bob.document("q3-report").insert(0, "Draft: ")

    team.client._stub = _Racing(team.client._stub, race)
    report.append(" Costs fell.")
    assert raced
    assert report.text() == "Draft: Sales rose 4%. Costs fell."
    assert report.written_by() == [("Draft: ", "bob"), ("Sales rose 4%. Costs fell.", "alice")]


# Nothing to add is nothing done. It does not start a document.
def test_adding_no_text_does_nothing(team):
    team.document("q3-report").append("")
    team.document("q3-report").insert(0, "")
    assert team.documents() == [] and team.index() == {}
