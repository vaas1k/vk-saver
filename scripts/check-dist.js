import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

const MARKER = 'VkSaverFake';
const dist = process.argv[2] ?? 'dist';

// A marker missing from fake.js would make this guard pass for nothing.
if (!readFileSync('src/dev/fake.js', 'utf8').includes(MARKER)) {
  process.stderr.write(`check-dist: marker ${MARKER} is not in src/dev/fake.js\n`);
  process.exit(1);
}
const hits = readdirSync(dist, { recursive: true })
  .map((f) => join(dist, f))
  .filter((p) => statSync(p).isFile() && readFileSync(p, 'latin1').includes(MARKER));
if (hits.length) {
  process.stderr.write(`check-dist: dev fake backend found in ${hits.join(', ')}\n`);
  process.exit(1);
}
