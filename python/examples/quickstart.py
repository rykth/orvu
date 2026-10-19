"""The shortest program that uses Orvu. Run it twice: the second run sees
what the first one wrote.

    pip install orvu
    python quickstart.py
"""

import orvu

with orvu.connect() as client:
    visits = client.counter("visits")
    visits.increment(1)
    print(f"this program has been run {visits.value()} time(s)")

    notes = client.list("notes")
    notes.insert(notes.len(), f"run number {visits.value()}")
    print("notes so far:", notes.to_list())
