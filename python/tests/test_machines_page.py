"""MACHINES.md, the guide to a team on several machines, against the real
`orvud`."""

import pathlib
import re
import subprocess

import orvu
from conftest import orvud_bin, wait_until

PAGE = pathlib.Path(__file__).resolve().parents[1] / "MACHINES.md"
TEXT = PAGE.read_text()


def blocks(kind):
    return re.findall(rf"```{kind}\n(.*?)```", TEXT, flags=re.S)


def commands(start):
    """Every command on the page that starts with `start`, as its list of
    words."""
    lines = [line.strip() for block in blocks("text") for line in block.splitlines()]
    return [line.split() for line in lines if line.startswith(start)]


def setups():
    """Every `orvud setup` command on the page that makes a team, as its
    list of words."""
    # The first is the team the page builds. Its answer, which is on the page
    # too, ends with the same command with the numbers written out.
    found = [command for command in commands("orvud setup ") if "--add" not in command]
    found = [command for command in found if command[2] != "<new-folder>"]
    assert len(found) == 3
    return found


def run(command, folder):
    return subprocess.run([orvud_bin(), *command[1:]], cwd=folder, capture_output=True, text=True)


def test_every_setup_command_on_the_page_works(tmp_path):
    for number, command in enumerate(setups()):
        folder = tmp_path / str(number)
        folder.mkdir()
        if "--from" in command:
            # Made again from the team the page builds first.
            assert run(setups()[0], folder).returncode == 0
        done = run(command, folder)
        assert done.returncode == 0, done.stderr
        machines = sorted(word.split("=")[0] for word in command[3:] if "=" in word)
        assert sorted(path.name for path in (folder / command[2]).iterdir()) == sorted(["authority", *machines])


# The machine the page adds is added to the team the page builds.
def test_the_machine_the_page_adds_is_added(tmp_path):
    (add,) = [command for command in commands("orvud setup crew --add") if "<name>=<address>" not in command]
    assert run(setups()[0], tmp_path).returncode == 0
    done = run(add, tmp_path)
    assert done.returncode == 0, done.stderr
    assert "\nreplica = 4\n" in (tmp_path / "crew" / "delta" / "orvu.toml").read_text()
    assert '"10.0.0.8:7440"' in (tmp_path / "crew" / "alpha" / "orvu.toml").read_text()
    # The page names the files the command makes.
    for name in ["orvu.toml", "node.pem", "node-key.pem", "ca.pem", "orvud-alpha.service"]:
        assert f"`{name}`" in TEXT, name
        assert (tmp_path / "crew" / "alpha" / name).exists(), name
    for line in commands("systemctl --user"):
        assert line[-1] in ("~/orvu/beta/orvud-beta.service", "orvud-beta.service"), line
        assert (tmp_path / "crew" / "beta" / "orvud-beta.service").exists()


# The page shows what the command answers and the settings file it writes.
# Both are compared with the real thing, word for word.
def test_the_page_shows_what_the_command_really_says_and_writes(tmp_path):
    done = run(setups()[0], tmp_path)
    assert done.stdout in blocks("text"), done.stdout
    (shown,) = blocks("toml")
    assert shown == (tmp_path / "crew" / "alpha" / "orvu.toml").read_text()


# Taking a machine out, as the page shows it, leaves the others their numbers.
def test_the_machines_that_stay_keep_their_numbers(tmp_path):
    (without_beta,) = [command for command in setups() if command[2] == "crew2"]
    assert run(setups()[0], tmp_path).returncode == 0
    assert run(without_beta, tmp_path).returncode == 0
    settings = (tmp_path / "crew2" / "gamma" / "orvu.toml").read_text()
    assert "\nreplica = 3\n" in settings
    # A machine added to the new team never gets beta's number.
    added = run(["orvud", "setup", "crew2", "--add", "delta=10.0.0.8"], tmp_path)
    assert added.returncode == 0, added.stderr
    assert "\nreplica = 4\n" in (tmp_path / "crew2" / "delta" / "orvu.toml").read_text()


def test_the_command_that_starts_a_sidecar_is_the_one_the_setup_names(tmp_path):
    said = run(setups()[0], tmp_path).stdout
    starts = [block.strip() for block in blocks("text") if block.startswith("orvud --config")]
    assert starts == ["orvud --config orvu.toml --socket orvud.sock"]
    # The service file starts it the same way, from the folder's own place.
    unit = (tmp_path / "crew" / "alpha" / "orvud-alpha.service").read_text()
    assert "--config %h/orvu/alpha/orvu.toml --socket %h/orvu/alpha/orvud.sock" in unit
    assert f"\n  {starts[0]}\n" in said


# The program on the page is run as it stands, against one machine of a real
# team, and another machine has to see what it wrote.
def test_the_program_on_the_page_runs_and_another_machine_sees_what_it_wrote(machines):
    (example,) = blocks("python")
    assert example.count('"/home/me/crew/orvud.sock"') == 1
    machines.start()
    exec(example.replace('"/home/me/crew/orvud.sock"', repr(str(machines.sidecars["alpha"].socket))), {})
    with orvu.Team.connect("research", socket=machines.sidecars["beta"].socket) as elsewhere:
        assert wait_until(lambda: elsewhere.notes.get("deploy-date") == "Friday")


def test_the_page_points_at_files_that_exist():
    names = re.findall(r"\]\(([A-Z]+\.md)\)", TEXT)
    assert names
    for name in names:
        assert (PAGE.parent / name).exists(), name
