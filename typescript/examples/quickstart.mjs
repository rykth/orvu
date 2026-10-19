// The shortest program that uses Orvu. Run it twice: the second run sees what
// the first one wrote.
//
//     npm install orvu
//     node quickstart.mjs

import { connect } from "orvu";

const client = await connect();

const visits = client.counter("visits");
await visits.increment(1);
console.log(`this program has been run ${await visits.value()} time(s)`);

const notes = client.list("notes");
await notes.insert(await notes.length(), `run number ${await visits.value()}`);
console.log("notes so far:", await notes.toArray());

client.close();
