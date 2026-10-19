"""The record of who changed what, as an agent's changes reach it. The
sidecar keeps it; `orvud audit` shows it."""

import subprocess

import orvu

from conftest import orvud_bin, wait_until


def audit(sidecar, *flags):
    """What `orvud audit` shows for this sidecar."""
    done = subprocess.run(
        [orvud_bin(), "audit", "--local", "--data-dir", sidecar.data, *flags],
        capture_output=True, text=True, check=True,
    )
    return done.stdout


def test_an_agents_changes_are_recorded_by_name_and_not_by_what_they_say(sidecar):
    with orvu.Team.connect(socket=sidecar.socket, key=sidecar.add_agent("alice")) as alice:
        alice.notes.put("deploy", "the launch code")
        alice.notes.get("deploy")
    assert wait_until(lambda: "key=deploy" in audit(sidecar))
    shown = audit(sidecar, "--agent", "alice")
    assert "alice  mv_map_put  orvu/default/notes-mv  key=deploy  done" in shown
    assert "the launch code" not in shown
    # Reading the note left no line.
    assert "map_get" not in shown


def test_what_an_agent_was_not_allowed_to_do_is_recorded(sidecar):
    key = sidecar.add_agent("reader")
    sidecar.allow("reader", "notes:read")
    with orvu.Team.connect(socket=sidecar.socket, key=key) as reader:
        try:
            reader.notes.put("deploy", "Monday")
        except orvu.NotAllowed:
            pass
    assert wait_until(lambda: "not allowed" in audit(sidecar, "--not-allowed"))
    # The first write of a note adds the notes to the team's list first.
    # That is the request the record names.
    assert "reader  map_put  orvu/default/index  key=notes-mv  not allowed: reader may not change notes" in audit(
        sidecar, "--not-allowed"
    )


# A budget's spending is recorded with the amount, so that who spent what
# can be told from the record.
def test_spending_is_recorded_with_the_amount(sidecar):
    with orvu.Team.connect(socket=sidecar.socket, key=sidecar.add_agent("alice")) as alice:
        alice.budget("tokens").top_up(100)
        alice.budget("tokens").spend(30)
        alice.budget("tokens").spend(500)
    assert wait_until(lambda: "refused" in audit(sidecar))
    shown = audit(sidecar, "--object", "orvu/default/budget/")
    assert "bounded_increment  orvu/default/budget/tokens  n=100  done" in shown
    assert "bounded_try_decrement  orvu/default/budget/tokens  n=30  done" in shown
    assert "bounded_try_decrement  orvu/default/budget/tokens  n=500  refused" in shown
