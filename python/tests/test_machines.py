"""One team on three machines. The three sidecars run on this computer, each
set up by `orvud setup`, and talk to each other over the real encrypted link."""

import re
import time
import urllib.request

import pytest

import orvu
from conftest import wait_until
from ports import free_port


def test_what_one_machine_writes_the_others_read(machines):
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)

    alpha.notes.put("deploy-date", "Friday")
    assert wait_until(lambda: beta.notes.get("deploy-date") == gamma.notes.get("deploy-date") == "Friday")

    task = beta.tasks.add("Summarise the Q3 report")
    assert wait_until(lambda: gamma.tasks.get(task) is not None and alpha.tasks.get(task) is not None)
    assert gamma.tasks.claim(task)
    assert wait_until(lambda: alpha.tasks.get(task).claimed_by == "gamma")
    # The others see that it is taken, and leave it alone.
    assert wait_until(lambda: beta.tasks.get(task).claimed_by == "gamma")
    assert not beta.tasks.claim(task)
    gamma.tasks.finish(task, result="It went up.")
    assert wait_until(lambda: alpha.tasks.get(task).state == "done")
    assert alpha.tasks.get(task).result == "It went up."

    alpha.budget("tokens").top_up(900)
    assert wait_until(lambda: gamma.budget("tokens").left() == 900)
    assert gamma.budget("tokens").spend(100) and beta.budget("tokens").spend(100)
    assert wait_until(lambda: alpha.budget("tokens").left() == 700)
    assert wait_until(lambda: sorted(gamma.budgets()) == ["tokens"])


# Messages, documents, tallies and the roster cross between machines as
# notes and tasks do.
def test_the_other_blocks_are_shared_between_machines_too(machines):
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)

    alpha.inbox.send("gamma", "The Q3 numbers are in the notes.")
    beta.inbox.send_all("Deploy is on Friday.")
    assert wait_until(lambda: len(gamma.inbox.messages()) == 2)
    assert sorted(message.sender for message in gamma.inbox.unread()) == ["alpha", "beta"]
    assert wait_until(lambda: [message.body for message in alpha.inbox.messages()] == ["Deploy is on Friday."])

    alpha.document("q3-report").append("Sales rose 4%.")
    assert wait_until(lambda: beta.document("q3-report").text() == "Sales rose 4%.")
    beta.document("q3-report").append(" Costs fell.")
    assert wait_until(
        lambda: gamma.document("q3-report").written_by() == [("Sales rose 4%.", "alpha"), (" Costs fell.", "beta")]
    )

    for team in (alpha, beta, gamma):
        team.tally("done").add(2)
        team.roster.join()
    assert wait_until(lambda: gamma.tally("done").by_name() == {"alpha": 2, "beta": 2, "gamma": 2})
    assert wait_until(lambda: alpha.roster.here() == ["alpha", "beta", "gamma"])

    alpha.notes.put("limit", {"tokens": 5000, "strict": True})
    assert wait_until(lambda: gamma.notes.get("limit") == {"tokens": 5000, "strict": True})
    assert wait_until(lambda: gamma.notes.author("limit") == "alpha")


# The machine is ended with no warning, and what it had not been sent is
# waiting for it when it is back.
def test_a_machine_that_was_away_catches_up_when_it_is_back(machines):
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)
    gamma.notes.put("from-gamma", "before")
    assert wait_until(lambda: alpha.notes.get("from-gamma") == "before")

    machines.kill("gamma")
    alpha.notes.put("deploy-date", "Friday")
    task = beta.tasks.add("Summarise the Q3 report")
    # The two that are left go on working with each other.
    assert wait_until(lambda: beta.notes.get("deploy-date") == "Friday")
    assert wait_until(lambda: alpha.tasks.get(task) is not None)
    assert alpha.tasks.claim(task)

    machines.start("gamma")
    gamma = machines.team("gamma")
    # Its own last write may not have reached its disk before it was ended.
    # The others have it, and give it back.
    assert wait_until(lambda: gamma.notes.get("from-gamma") == "before", 20)
    assert wait_until(lambda: gamma.notes.get("deploy-date") == "Friday", 20)
    assert wait_until(lambda: gamma.tasks.get(task) is not None and gamma.tasks.get(task).claimed_by == "alpha", 20)
    # And it is heard again.
    gamma.notes.put("from-gamma", "after")
    assert wait_until(lambda: alpha.notes.get("from-gamma") == beta.notes.get("from-gamma") == "after", 20)


# A machine that lists nobody waits to be found. The others found it once,
# and have to find it again after it has been away.
def test_a_machine_that_dials_nobody_is_found_again_after_it_was_away(machines):
    settings = machines.settings("gamma")
    listed = [line for line in settings.read_text().splitlines() if line.startswith("peers = ")]
    assert len(listed) == 1
    settings.write_text(settings.read_text().replace(listed[0], "peers = []"))

    # Gamma is up first, so the first dial of each of the others reaches it.
    # A dial that began before gamma was up could still be under way later,
    # and would find gamma again by chance.
    machines.start("gamma")
    machines.start("alpha", "beta")
    alpha, beta, gamma = (machines.team(name) for name in machines.names)
    alpha.notes.put("deploy-date", "Friday")
    beta.notes.put("owner", "beta")
    # Gamma has heard from both.
    assert wait_until(lambda: gamma.notes.get("deploy-date") == "Friday" and gamma.notes.get("owner") == "beta")

    machines.stop("gamma")
    alpha.notes.put("while-away", "one")
    machines.start("gamma")
    gamma = machines.team("gamma")
    assert wait_until(lambda: gamma.notes.get("while-away") == "one", 15), "nobody dialed it again"


# The settings `orvud setup` writes share a budget out between the machines.
# So when one machine is away the others can still spend their shares, and
# only the share of the one that is away has to wait for it.
def test_most_of_a_budget_can_be_spent_while_a_machine_is_away(machines):
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)

    def spend_all(team):
        spent = 0
        while team.budget("tokens").spend(50):
            spent += 50
        return spent

    alpha.budget("tokens").top_up(900)
    # The sharing out happens once a second. This is time for a few rounds.
    time.sleep(5)
    machines.kill("alpha")

    spent = spend_all(beta) + spend_all(gamma)
    assert spent == 600, "the two that are left could not spend their two thirds"
    assert beta.budget("tokens").left() == 300

    # The rest was with alpha all along, and nothing was spent twice.
    machines.start("alpha")
    alpha = machines.team("alpha")
    assert wait_until(lambda: alpha.budget("tokens").left() == 300, 20)
    assert spend_all(alpha) == 300
    assert wait_until(lambda: beta.budget("tokens").left() == 0, 20)
    assert not beta.budget("tokens").spend(1)


def test_a_machine_stops_at_once_while_another_is_away(machines):
    machines.start()
    machines.team("alpha").notes.put("deploy-date", "Friday")
    machines.kill("gamma")
    # Long enough for the others to be in the middle of dialing it again.
    time.sleep(1)
    asked = time.monotonic()
    machines.stop("alpha")
    assert time.monotonic() - asked < 5, "it waited for the machine that is away"
    assert "orvud: stopped" in machines.sidecars["alpha"].printed()


def test_a_program_that_gives_no_socket_is_told_to_give_one(machines, home):
    # `orvu.connect()` is for the sidecar that keeps its data on this machine
    # only. A machine of a team is never picked up by accident.
    running = machines.sidecars["alpha"]
    running.socket = home / "run" / "orvud.sock"
    machines.start("alpha")
    with pytest.raises(orvu.SidecarUnavailable, match="give its socket"):
        orvu.Team.connect("crew")
    with orvu.Team.connect("crew", socket=running.socket, me="alice") as team:
        team.notes.put("deploy-date", "Friday")


def peers_up(port):
    """How many other machines the sidecar whose numbers are at `port` is
    connected to."""
    try:
        page = urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=5).read().decode()
    except OSError:
        return None
    found = re.search(r"^orvu_peers_up (\S+)$", page, re.M)
    return int(float(found.group(1))) if found else None


def test_a_machine_whose_cable_was_pulled_is_found_again(machines):
    # Short limits for the links, so this takes seconds and not minutes. The
    # limit on a silent connected machine is set far out of reach, so it is
    # the link's own idle time that has to notice the frozen machine.
    port = free_port()
    for name in machines.names:
        settings = machines.settings(name)
        extra = "\n[link]\nidle_ms = 2000\nkeep_alive_ms = 500\ndial_ms = 2000\nquiet_ms = 600000\n"
        if name == "alpha":
            extra += f'[metrics]\naddr = "127.0.0.1:{port}"\n'
        settings.write_text(settings.read_text() + extra)
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)
    alpha.notes.put("deploy-date", "Friday")
    assert wait_until(lambda: gamma.notes.get("deploy-date") == "Friday")
    assert wait_until(lambda: peers_up(port) == 2)

    machines.freeze("gamma")
    alpha.notes.put("while-away", "one")
    assert wait_until(lambda: peers_up(port) == 1, 15), "the frozen machine was never given up"
    machines.thaw("gamma")

    assert wait_until(lambda: gamma.notes.get("while-away") == "one", 30), "it never caught up"
    assert wait_until(lambda: peers_up(port) == 2, 15)
    gamma.notes.put("back", "three")
    assert wait_until(lambda: alpha.notes.get("back") == beta.notes.get("back") == "three", 30)
