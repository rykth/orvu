"""The sidecar's log and its page of numbers, turned on by a small settings
file in the data folder."""

import re
import time
import urllib.request

import pytest

from conftest import Sidecar, short_folder
from ports import free_port

import orvu


def number(page, name):
    """The value on the line of the page that starts with `name`."""
    found = re.search("^" + re.escape(name) + r" (\S+)$", page, re.M)
    assert found, f"{name} is not on the page"
    return float(found.group(1))


def test_the_page_counts_what_a_team_does_and_the_log_says_so():
    with short_folder() as folder:
        running = Sidecar(folder)
        running.data.mkdir(mode=0o700)
        port = free_port()
        (running.data / "orvud.toml").write_text(f'[logs]\nlevel = "info"\n[metrics]\naddr = "127.0.0.1:{port}"\n')
        running.start()
        try:
            with orvu.Team.connect(socket=running.socket, me="alice") as team:
                team.notes.put("plan", "Ship it")
                team.notes.put("owner", "alice")
                with team.client.batch() as batch:
                    batch.counter("visits").increment(1)
                page = urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=5).read().decode()
        finally:
            running.stop()
        printed = running.printed()
    # A team writes some bookkeeping of its own as well, so these are at
    # least what the test did.
    assert number(page, 'orvu_sidecar_operations_total{op="mv_map_put",result="done"}') == 2
    assert number(page, 'orvu_sidecar_operations_total{op="counter_increment",result="done"}') == 1
    assert number(page, "orvu_sidecar_batch_seconds_count") >= 1
    assert number(page, "orvu_sidecar_connections") == 1
    # The engine's numbers are on the same page.
    assert "orvu_objects " in page
    # With no file given, the log goes to standard error, next to the line
    # the sidecar always prints.
    assert "INFO" in printed and "orvud serving" in printed


def test_with_no_settings_file_there_is_no_log(sidecar):
    with orvu.Team.connect(socket=sidecar.socket, me="alice") as team:
        team.notes.put("plan", "Ship it")
    assert "INFO" not in sidecar.printed()


# A mistake in the settings file of a sidecar a program starts by itself is
# reported at once, and says where it is.
def test_a_mistake_in_the_settings_file_is_reported_at_once(home):
    data = home / ".local" / "share" / "orvu"
    data.mkdir(mode=0o700, parents=True)
    (data / "orvud.toml").write_text('[logs]\nlevel = "loud"\n')
    started = time.monotonic()
    with pytest.raises(orvu.SidecarUnavailable, match="orvud.toml"):
        orvu.connect(timeout=30)
    assert time.monotonic() - started < 10
