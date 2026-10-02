// Time a Vite dev server's first page in the project in DIR, ROUNDS times: start `vite dev`, wait
// until it's ready, then load the page like a browser: the HTML (rendered on the server for
// SvelteKit), then every module it imports, six requests at a time. Each round starts a new
// server, so the server-side modules are loaded and transformed again; Vite's dependency cache in
// node_modules/.vite stays (discard the first round after it was cleared). Run it in the app's own
// processes (SSH or the in-app terminal), like proot-workloads.sh.
//
// Usage: node vite-first-page.mjs DIR [ROUNDS]   (SHOW_FAILED=1 lists failed requests; a few 404s
// are type annotations in comments that look like imports)
import { spawn } from 'node:child_process';

const [dir, rounds = '3'] = process.argv.slice(2);
const origin = 'http://localhost:5173';
const importRe =
	/(?:^|[;\n}])\s*(?:import|export)\s*(?:[^'";]*?\sfrom\s*)?["']([^"']+)["']|import\(\s*["']([^"']+)["']\s*\)/g;

async function loadPage() {
	const t0 = performance.now();
	const seen = new Set();
	const queue = [];
	const add = (spec, from) => {
		if (!spec || !(spec.startsWith('/') || spec.startsWith('.'))) return;
		const url = new URL(spec, from).href;
		if (!url.startsWith(origin) || seen.has(url)) return;
		seen.add(url);
		queue.push(url);
	};
	const html = await (await fetch(origin + '/')).text();
	const tHtml = performance.now() - t0;
	for (const m of html.matchAll(/<script[^>]*\bsrc="([^"]+)"/g)) add(m[1], origin + '/');
	for (const m of html.matchAll(/<link[^>]*rel="modulepreload"[^>]*href="([^"]+)"/g)) add(m[1], origin + '/');
	for (const m of html.matchAll(/import\(\s*["']([^"']+)["']\s*\)/g)) add(m[1], origin + '/');

	let count = 0;
	let failed = 0;
	const fetchOne = async (url) => {
		const response = await fetch(url);
		const body = await response.text();
		count++;
		if (!response.ok) {
			failed++;
			if (process.env.SHOW_FAILED) console.log(response.status, url);
			return;
		}
		if ((response.headers.get('content-type') ?? '').includes('javascript')) {
			for (const m of body.matchAll(importRe)) add(m[1] ?? m[2], url);
		}
	};
	await new Promise((resolve, reject) => {
		let active = 0;
		const pump = () => {
			while (active < 6 && queue.length) {
				active++;
				fetchOne(queue.shift())
					.then(() => {
						active--;
						if (!queue.length && !active) resolve();
						else pump();
					})
					.catch(reject);
			}
		};
		pump();
		if (!queue.length && !active) resolve();
	});
	return { html: tHtml, all: performance.now() - t0, count, failed };
}

for (let round = 0; round < Number(rounds); round++) {
	const t0 = performance.now();
	const vite = spawn('node_modules/.bin/vite', ['dev', '--port', '5173', '--strictPort'], {
		cwd: dir,
		stdio: ['ignore', 'pipe', 'pipe']
	});
	let output = '';
	await new Promise((resolve, reject) => {
		const onData = (data) => {
			output += data;
			if (/ready in/.test(output)) resolve();
		};
		vite.stdout.on('data', onData);
		vite.stderr.on('data', onData);
		vite.on('exit', (code) => reject(new Error(`vite exited with ${code}:\n${output}`)));
	});
	const ready = performance.now() - t0;
	const page = await loadPage();
	const exited = new Promise((resolve) => vite.on('exit', resolve));
	vite.kill('SIGTERM');
	await exited;
	const s = (ms) => (ms / 1000).toFixed(2);
	console.log(
		`ready ${s(ready)} s, html ${s(page.html)} s, page ${s(page.all)} s ` +
			`(${page.count} requests, ${page.failed} failed)`
	);
}
