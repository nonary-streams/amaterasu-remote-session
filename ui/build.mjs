import { spawnSync } from 'node:child_process';
import { access, copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const sdkRoot = process.env.AMATERASU_SDK
  ? resolve(process.env.AMATERASU_SDK)
  : resolve(root, '../../..');
const declarations = resolve(sdkRoot, 'sdk/web/amaterasu-ui.d.ts');
try {
  await access(declarations);
} catch {
  throw new Error('Set AMATERASU_SDK to an Amaterasu checkout containing sdk/web/amaterasu-ui.d.ts.');
}
await mkdir(resolve(root, 'build'), { recursive: true });
await writeFile(resolve(root, 'build/tsconfig.json'), JSON.stringify({
  compilerOptions: {
    target: 'ES2022', module: 'none', lib: ['ES2022', 'DOM', 'DOM.Iterable'],
    strict: true, noUncheckedIndexedAccess: true, removeComments: true,
    outFile: './remote-session.js', noEmit: process.argv.includes('--check'),
  },
  files: [declarations, resolve(root, 'src/settings.ts'), resolve(root, 'src/main.ts')],
}, null, 2) + '\n');
const result = spawnSync(process.execPath,
  [resolve(root, 'node_modules/typescript/bin/tsc'), '--project', resolve(root, 'build/tsconfig.json')],
  { cwd: root, stdio: 'inherit' });
if (result.status !== 0) process.exit(result.status ?? 1);
if (!process.argv.includes('--check')) {
  await mkdir(resolve(root, 'dist'), { recursive: true });
  const compiled = await readFile(resolve(root, 'build/remote-session.js'), 'utf8');
  await writeFile(resolve(root, 'dist/remote-session.js'), `(function(){\n${compiled}\n})();\n`);
  await copyFile(resolve(root, 'remote-session.css'), resolve(root, 'dist/remote-session.css'));
}
