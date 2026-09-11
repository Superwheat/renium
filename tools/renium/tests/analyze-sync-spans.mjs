// Diagnostic trace reader. Durations are wall time; neither sums across worker
// threads nor bridge waits represent CPU time or an end-to-end critical path.
import assert from 'node:assert/strict';
import fs from 'node:fs';

export function analyze(events) {
  const threads = new Map();
  for (const event of events) {
    assert.ok(event.ph === 'X' && Number.isFinite(event.ts) && event.dur >= 0);
    const key = `${event.pid}:${event.tid}`;
    if (!threads.has(key)) threads.set(key, []);
    threads.get(key).push({...event, children: [], end: event.ts + event.dur});
  }
  const rows = [];
  for (const timeline of threads.values()) {
    timeline.sort((a,b) => a.ts - b.ts || b.dur - a.dur);
    const stack = [];
    for (const event of timeline) {
      while (stack.length && event.end > stack.at(-1).end) stack.pop();
      while (stack.length && event.ts >= stack.at(-1).end) stack.pop();
      if (stack.length) stack.at(-1).children.push(event);
      stack.push(event);
    }
    for (const event of timeline) {
      let covered = 0;
      let last = event.ts;
      for (const child of event.children) {
        covered += Math.max(0, child.end - Math.max(last, child.ts));
        last = Math.max(last, child.end);
      }
      rows.push({name: event.name, category: event.cat, pid: event.pid, tid: event.tid,
        requestId: event.args?.request_id, contextId: event.args?.context_id,
        startUs: event.ts, inclusiveMs: event.dur / 1000,
        selfOrUnattributedMs: (event.dur - covered) / 1000});
    }
  }
  return rows.sort((a,b) => b.selfOrUnattributedMs - a.selfOrUnattributedMs);
}

export function selectOperation(events, name, commandEvents) {
  const command = commandEvents?.filter(event => event.cat === 'cli.process' || event.cat === 'cli')
    .sort((a,b) => b.dur - a.dur)[0];
  if (commandEvents) assert.ok(command, 'Command log has no CLI span');
  const key = event => `${event.args?.request_id}:${event.args?.context_id}:${event.name}`;
  const requests = new Set(commandEvents?.filter(event => event.cat === 'daemon.rpc' && event.args).map(key));
  const root = events.filter(event => event.cat === 'daemon'
    && (requests.size ? requests.has(key(event))
      : command ? event.ts >= command.ts && event.ts + event.dur <= command.ts + command.dur
      : event.name === name)).sort((a,b) => command ? b.dur - a.dur : b.ts - a.ts)[0];
  assert.ok(root, `No completed daemon operation matching ${command?.name ?? name}`);
  const inWindow = event => event.pid === root.pid && event.ts >= root.ts
    && event.ts + event.dur <= root.ts + root.dur;
  const candidates = events.filter(inWindow);
  const selected = candidates.filter(event => root.args?.request_id === undefined
    ? event.tid === root.tid
    : event.args?.request_id === root.args.request_id
      && event.args?.context_id === root.args.context_id);
  return {root, events: selected,
    uncorrelatedSpans: candidates.filter(event => event !== root && !event.args && event.tid !== root.tid).length};
}

// Partition ONE wall-clock envelope. Parallel workers are reported separately;
// selecting their longest span does not establish which worker delayed a join.
export function accountTimeline(root, events) {
  const end = root.ts + root.dur;
  const spans = events.filter(event => event !== root && event.ph === 'X'
    && event.pid === root.pid && event.tid === root.tid
    && event.ts >= root.ts && event.ts + event.dur <= end && event.dur > 0);
  const boundaries = [...new Set([root.ts, end,
    ...spans.flatMap(event => [event.ts, event.ts + event.dur])])].sort((a,b) => a-b);
  const segments = [];
  for (let i = 1; i < boundaries.length; ++i) {
    const start = boundaries[i-1], finish = boundaries[i];
    const active = spans.filter(event => event.ts <= start && event.ts + event.dur >= finish)
      .sort((a,b) => a.dur-b.dur || b.ts-a.ts);
    const leaf = active[0];
    const name = leaf?.name ?? `UNINSTRUMENTED: ${root.name}`;
    const category = leaf?.cat ?? 'unaccounted';
    const previous = segments.at(-1);
    if (previous?.name === name && previous.category === category
      && previous.requestId === leaf?.args?.request_id && previous.contextId === leaf?.args?.context_id
      && previous.sourceStartUs === leaf?.ts && previous.sourceEndUs === (leaf && leaf.ts+leaf.dur)) previous.endUs = finish;
    else segments.push({name, category, startUs: start, endUs: finish,
      sourceStartUs: leaf?.ts, sourceEndUs: leaf && leaf.ts+leaf.dur,
      requestId: leaf?.args?.request_id, contextId: leaf?.args?.context_id});
  }
  const ranked = new Map();
  for (const segment of segments) {
    segment.wallMs = (segment.endUs-segment.startUs)/1000;
    const key = `${segment.category}:${segment.name}`;
    const row = ranked.get(key) ?? {name: segment.name, category: segment.category, wallMs: 0};
    row.wallMs += segment.wallMs;
    ranked.set(key, row);
  }
  const rows = [...ranked.values()].sort((a,b) => b.wallMs-a.wallMs);
  const accountedMs = rows.reduce((sum,row) => sum+row.wallMs, 0);
  assert.ok(Math.abs(accountedMs-root.dur/1000) < 1e-6, 'Wall-clock accounting does not balance');
  const unaccountedMs = rows.filter(row => row.category === 'unaccounted')
    .reduce((sum,row) => sum+row.wallMs,0);
  return {name: root.name, wallMs: root.dur/1000, balancedMs: accountedMs,
    unaccountedMs, complete: unaccountedMs === 0, rows, segments};
}

export function commandBreakdown(command, commandEvents, daemonEvents, profiles) {
  const timeline = accountTimeline(command, commandEvents);
  const rows = [], warnings = [];
  const daemonOverlap = new Map();
  const add = (name, category, wallMs) => { if (wallMs > 0) rows.push({name, category, wallMs}); };
  for (const segment of timeline.segments) {
    if (segment.category !== 'daemon.wait' || segment.requestId === undefined) {
      add(segment.name, segment.category, segment.wallMs);
      continue;
    }
    const root = daemonEvents.filter(event => event.cat === 'daemon'
      && event.args?.request_id === segment.requestId && event.args?.context_id === segment.contextId)
      .sort((a,b) => b.dur-a.dur)[0];
    const exchange = commandEvents.find(event => event.cat === 'daemon.rpc'
      && event.args?.request_id === segment.requestId && event.args?.context_id === segment.contextId
      && event.ts <= segment.startUs && event.ts+event.dur >= segment.endUs);
    // The daemon can begin while the CLI finishes sending/logging the request.
    // Validate against the entire exchange, then replace only its wait overlap.
    const outerStart = exchange?.ts ?? segment.startUs;
    const outerEnd = exchange ? exchange.ts+exchange.dur : segment.endUs;
    if (!root || root.ts < outerStart || root.ts+root.dur > outerEnd) {
      warnings.push(`Missing or clock-inconsistent daemon span for request ${segment.requestId}`);
      add(segment.name, 'unaccounted.remote', segment.wallMs);
      continue;
    }
    const overlapStart = Math.max(root.ts,segment.startUs);
    const overlapEnd = Math.min(root.ts+root.dur,segment.endUs);
    const overlapMs = Math.max(0,overlapEnd-overlapStart)/1000;
    if (!daemonOverlap.has(root)) daemonOverlap.set(root,[]);
    if (overlapMs > 0) daemonOverlap.get(root).push([overlapStart,overlapEnd]);
    add('socket transit, daemon dispatch and response return', 'transport', segment.wallMs-overlapMs);
    const children = daemonEvents.filter(event => event.args?.request_id === segment.requestId
      && event.args?.context_id === segment.contextId);
    const nativeProfiles = profiles.filter(profile => profile.name === 'native.import.accounting'
      && profile.pid === root.pid && profile.tid === root.tid
      && profile.context?.request_id === segment.requestId && profile.context?.context_id === segment.contextId);
    const hostSpans = children.filter(event => event.cat === 'native.host'
      && event.name === 'execute native helper and wait for completion');
    const used = new Set();
    for (const fullRow of accountTimeline(root,children).segments) {
      const startUs = Math.max(fullRow.startUs,overlapStart);
      const endUs = Math.min(fullRow.endUs,overlapEnd);
      if (endUs <= startUs) continue;
      const row = {...fullRow,startUs,endUs,wallMs:(endUs-startUs)/1000};
      if (row.category !== 'native.host' || row.name !== 'execute native helper and wait for completion') {
        add(row.name,row.category,row.wallMs);
        continue;
      }
      // Match individual calls, never the sum of equal-named calls in an RPC.
      // Legacy records are unambiguous only with exactly one call and profile.
      const candidates = nativeProfiles.filter(profile => {
        if (used.has(profile) || row.startUs !== row.sourceStartUs || row.endUs !== row.sourceEndUs) return false;
        const interval = profile.data.hostInterval;
        return interval ? interval.ts >= row.startUs && interval.ts+interval.dur <= row.endUs
          : nativeProfiles.length === 1 && hostSpans.length === 1
            && Math.abs(profile.data.wallMs-row.wallMs) <= 0.002;
      });
      if (candidates.length === 1) {
        const profile = candidates[0];
        used.add(profile);
        const native = profile.data;
        const phases = Object.entries(native.phasesMs);
        assert.ok(phases.every(([,ms])=>Number.isFinite(ms) && ms >= 0), 'Invalid native phase duration');
        assert.ok(phases.reduce((sum,[,ms])=>sum+ms,0) <= row.wallMs,
          'Native phases exceed their containing invocation');
        for (const [name, wallMs] of phases) add(name,'native.phase',wallMs);
        add('remote invocation, return and helper teardown','native.boundary',
          row.wallMs-phases.reduce((sum,[,ms])=>sum+ms,0));
      } else {
        warnings.push(`Unmatched native invocation at ${row.startUs}; ${candidates.length} candidate profiles`);
        add(row.name,'unaccounted.native',row.wallMs);
      }
    }
  }
  const grouped = new Map();
  for (const row of rows) {
    const key = `${row.category}:${row.name}`;
    const old = grouped.get(key);
    if (old) old.wallMs += row.wallMs; else grouped.set(key,{...row});
  }
  const result = [...grouped.values()].sort((a,b)=>b.wallMs-a.wallMs);
  const balancedMs = result.reduce((sum,row)=>sum+row.wallMs,0);
  assert.ok(Math.abs(balancedMs-command.dur/1000)<1e-6,'Combined command accounting does not balance');
  let concurrentDaemonMs = 0;
  for (const [root,ranges] of daemonOverlap) {
    let covered = 0, last = root.ts;
    for (const [start,end] of ranges.sort((a,b)=>a[0]-b[0])) {
      covered += Math.max(0,end-Math.max(last,start));
      last = Math.max(last,end);
    }
    concurrentDaemonMs += (root.dur-covered)/1000;
  }
  return {wallMs:command.dur/1000,balancedMs,warnings,rows:result,concurrentDaemonMs,
    unaccountedMs:result.filter(row=>row.category.startsWith('unaccounted')).reduce((sum,row)=>sum+row.wallMs,0),
    basis:'One CLI wall-clock envelope with the overlapping portion of correlated daemon waits replaced by daemon work, and native waits replaced by native phases. Concurrent CLI/daemon prefixes and worker details are not added twice.'};
}

function nativeAccounting(profiles) {
  return profiles.filter(profile => profile.name === 'native.import.accounting').map(({data}) => {
    const rows = Object.entries(data.phasesMs).map(([name, wallMs]) => ({name, wallMs}));
    rows.push({name: 'remote invocation, return and helper teardown', wallMs: data.invocationAndReturnMs});
    assert.ok(rows.every(row => Number.isFinite(row.wallMs) && row.wallMs >= 0),
      'Invalid native phase duration; cannot claim complete accounting');
    const measured = rows.reduce((sum,row) => sum+row.wallMs,0);
    assert.ok(Math.abs(measured-data.wallMs) < 0.001, 'Native phases do not cover native invocation');
    return {...data, rows: rows.sort((a,b) => b.wallMs-a.wallMs),
      warning: 'Factory/constructor times nest inside engine reads. Lua tracking spans the whole import observation window, including deferred callbacks between batches; do not subtract that aggregate from engine read or add it to these rows.'};
  });
}

function nativeBatchBreakdown(profiles) {
  const readers = profiles.filter(p=>p.name==='native.import.accounting');
  const batches = profiles.filter(p=>p.name==='native.import.batch');
  if (!batches.length) return undefined;
  // Existing batch records carry request ownership, not individual reader IDs.
  // Do not mix batch sequences when a request invokes more than one reader.
  if (readers.length !== 1) return {warning:'Multiple native readers: batch attribution requires per-reader IDs.'};
  const expected = batches[0].data.batches;
  const indices = new Set(batches.map(p=>p.data.index));
  assert.ok(batches.length===expected && indices.size===expected
    && batches.every(p=>p.data.batches===expected && Number.isInteger(p.data.index)
      && p.data.index>=1 && p.data.index<=expected && p.data.readMs>=0), 'Native batch sequence is incomplete');
  const groups = new Map();
  for (const {data} of batches) {
    const key = data.services.join(', ');
    const row = groups.get(key) ?? {services:data.services,batches:0,bytes:0,readMs:0,maxBatchMs:0};
    row.batches++;
    row.bytes += data.bytes;
    row.readMs += data.readMs;
    row.maxBatchMs = Math.max(row.maxBatchMs,data.readMs);
    groups.set(key,row);
  }
  const rows = [...groups.values()].sort((a,b)=>b.readMs-a.readMs);
  const readMs = rows.reduce((sum,row)=>sum+row.readMs,0);
  const engineMs = readers[0].data.phasesMs['Studio native deserialization'];
  assert.ok(readMs<=engineMs, 'Batch reads exceed their containing engine phase');
  return {rows,readMs,engineMs,boundaryAndClockMs:engineMs-readMs,
    basis:'Sequential native read calls grouped by their actual payload services. Mixed-service batches stay mixed; costs are not apportioned by instance counts. These times are inside the engine-read total, not additional.'};
}

function readRecords(log, marker) {
  return fs.readFileSync(log, 'utf8').split(/\r?\n/).flatMap(line => {
    const at = line.indexOf(marker);
    return at < 0 ? [] : [JSON.parse(line.slice(at + marker.length))];
  });
}

const readEvents = log => {
  const spans = readRecords(log, '[renium] span ');
  // Each next record carries the PREVIOUS write's completed interval. This
  // measures logging itself without recursively logging its own logger.
  const output = [...spans, ...readRecords(log,'[renium] profile ')].flatMap(record =>
    record.output ? [{...record.output, pid:record.pid, tid:record.tid,
      name:'encode and write trace record', cat:'trace.output', ph:'X'}] : []);
  return [...spans,...output];
};

if (process.argv[2] === '--self-test') {
  const event = (name, ts, dur, tid = 1) => ({name, ts, dur, tid, pid: 1, cat: 'test', ph: 'X'});
  const rows = analyze([event('root', 0, 100), event('child1', 10, 40),
    event('child2', 30, 40), event('nested', 35, 10), event('parallel', 0, 100, 2)]);
  assert.equal(rows.find(row => row.name === 'root').selfOrUnattributedMs, 0.04);
  assert.equal(rows.find(row => row.name === 'child2').selfOrUnattributedMs, 0.03);
  assert.equal(rows.find(row => row.name === 'parallel').selfOrUnattributedMs, 0.1);
  const root = {...event('push', 100, 100), cat: 'daemon', args: {request_id: 7, context_id: 2}};
  const worker = {...event('worker', 110, 30, 2), args: root.args};
  const other = {...event('other place', 110, 30, 3), args: {request_id: 8, context_id: 3}};
  const selection = selectOperation([root, worker, other, event('unknown worker', 110, 10, 4)], 'push');
  assert.deepEqual(selection.events, [root, worker]);
  assert.equal(selection.uncorrelatedSpans, 1);
  const command = {...event('ps', 95, 110), cat: 'cli', pid: 99};
  assert.equal(selectOperation([root, other], undefined, [command]).root, root);
  const rpc = {...event('push', 300, 100), cat: 'daemon.rpc', args: root.args};
  assert.equal(selectOperation([root, other], undefined, [{...command, ts: 300}, rpc]).root, root);
  const accounting = accountTimeline(event('root',0,100), [event('work',0,60),
    event('nested',10,20),event('wait',60,40), event('worker',0,100,2)]);
  assert.equal(accounting.complete,true);
  assert.equal(accounting.balancedMs,0.1);
  assert.equal(accounting.rows.find(row => row.name === 'work').wallMs,0.04);
  assert.equal(accountTimeline(event('root',0,100), [event('child',10,20)]).unaccountedMs,0.08);
  assert.throws(() => nativeAccounting([{name:'native.import.accounting',data:{wallMs:1,
    invocationAndReturnMs:-1,phasesMs:{engine:2}}}]));
  const envelope = {...event('command',0,100),pid:2,cat:'cli.process'};
  const waiting = {...event('read',10,80),pid:2,cat:'daemon.wait',args:root.args};
  const remote = {...root,ts:20,dur:60};
  const combined = commandBreakdown(envelope,[waiting], [remote,{
    ...event('remote work',20,60),args:root.args}],[]);
  assert.ok(Math.abs(combined.rows.find(row=>row.category==='transport').wallMs-0.02)<1e-9);
  assert.equal(combined.unaccountedMs,0.02);
  const early = commandBreakdown(envelope,[waiting,{...waiting,cat:'daemon.rpc',ts:5,dur:90}],
    [{...remote,ts:5,dur:80},{...event('early remote work',5,80),args:root.args}],[]);
  assert.equal(early.warnings.length,0);
  assert.ok(Math.abs(early.concurrentDaemonMs-0.005)<1e-9);
  assert.ok(Math.abs(early.rows.find(row=>row.name==='early remote work').wallMs-0.075)<1e-9);
  const request = {request_id:11,context_id:2};
  const host = (ts,dur) => ({...event('execute native helper and wait for completion',ts,dur),cat:'native.host',args:request});
  const twoCalls = [{...event('push',10,80),cat:'daemon',args:request},host(10,40),host(50,40)];
  const profile = (ts,name,wallMs) => ({name:'native.import.accounting',pid:1,tid:1,context:request,
    data:{hostInterval:{ts,dur:40},wallMs:0.04,phasesMs:{[name]:wallMs}}});
  // Same-duration adjacent calls must remain distinct and use their own profile.
  const twoProfiles = [profile(50,'second call',0.02),profile(10,'first call',0.03)];
  const two = commandBreakdown(envelope,[{...waiting,args:request}],twoCalls,twoProfiles);
  assert.equal(two.warnings.length,0);
  assert.equal(two.rows.find(row=>row.name==='first call').wallMs,0.03);
  assert.equal(two.rows.find(row=>row.name==='second call').wallMs,0.02);
  assert.ok(Math.abs(two.balancedMs-0.1)<1e-9);
  const ambiguous = commandBreakdown(envelope,[{...waiting,args:request}],twoCalls,
    twoProfiles.map(p=>({...p,data:{...p.data,hostInterval:undefined}})));
  assert.equal(ambiguous.warnings.length,2);
  assert.equal(ambiguous.rows.find(row=>row.category==='unaccounted.native').wallMs,0.08);
  console.log('Timing interval union, nesting and parallel separation passed.');
} else if (process.argv[2]) {
  const [log, prefix, ...options] = process.argv.slice(2);
  assert.ok(options.length === 0 || (options.length === 2
    && ['--operation', '--command-log'].includes(options[0])),
    'Optional filter: --operation NAME or --command-log CLI_TRACE_LOG');
  assert.ok(prefix, 'Provide log path and output prefix');
  let events = readEvents(log);
  const allDaemonEvents = events;
  const commandEvents = options[0] === '--command-log' ? readEvents(options[1]) : undefined;
  assert.ok(events.length, 'No timestamped spans in this log');
  const selection = options.length ? selectOperation(events, options[1],
    commandEvents) : undefined;
  if (selection) events = selection.events;
  const profiles = readRecords(log, '[renium] profile ').filter(profile => !selection
    || profile.pid === selection.root.pid
      && profile.context?.request_id === selection.root.args?.request_id
      && profile.context?.context_id === selection.root.args?.context_id);
  const rows = analyze(events);
  const command = commandEvents?.filter(event => event.cat === 'cli.process' || event.cat === 'cli')
    .sort((a,b) => b.dur-a.dur)[0];
  const accounting = selection && accountTimeline(selection.root, events);
  const native = nativeAccounting(profiles);
  const nativeBatches = nativeBatchBreakdown(profiles);
  const combined = command && commandBreakdown(command,commandEvents,allDaemonEvents,
    readRecords(log,'[renium] profile '));
  fs.writeFileSync(prefix + '.trace.json', JSON.stringify({traceEvents: events}));
  fs.writeFileSync(prefix + '.ranked.json', JSON.stringify({
    basis: 'Exclusive main-thread wall-clock partition. Worker and native detail are nested views, not additional command costs.',
    limitation: 'Concurrent worker rows and remote Studio work are not additive; residual is NOT CPU time.',
    operation: selection && {name: selection.root.name, wallMs: selection.root.dur / 1000,
      uncorrelatedSpans: selection.uncorrelatedSpans,
      mainThread: rows.filter(row => row.tid === selection.root.tid)},
    profiles,
    accounting,
    commandAccounting: command && accountTimeline(command, commandEvents),
    nativeAccounting: native,
    nativeBatchBreakdown: nativeBatches,
    combinedCommandAccounting: combined,
    coverageWarnings: [
      ...(accounting?.unaccountedMs > 0 ? ['Uninstrumented intervals remain; see accounting.segments.'] : []),
      ...(selection?.uncorrelatedSpans ? ['Uncorrelated worker spans exist; they are excluded.'] : []),
      ...(profiles.some(p => p.name === 'native.import.reader') && !native.length
        ? ['Native reader log predates complete queue/pacing/receipt accounting.'] : []),
      'A named parent stage is not a detailed engine diagnosis. Native engine internals and other-plugin callbacks still require profiler evidence.',
    ],
    rows,
  }, null, 2));
  if (combined) {
    const escape = value => String(value).replaceAll('|','\\|').replaceAll('\n',' ');
    const report = ['# Full command timing', '',
      `CLI execution: **${combined.wallMs.toFixed(3)} ms**. Uninstrumented: **${combined.unaccountedMs.toFixed(3)} ms**.`, '',
      combined.basis, '',
      'OS process startup/exit and the calling shell are outside the CLI envelope; report the separate harness stopwatch too.', '',
      `Daemon work overlapping non-waiting CLI work: ${combined.concurrentDaemonMs.toFixed(3)} ms (nested, not additional).`, '',
      '| Stage (exclusive wall time) | Category | ms |', '|---|---|---:|',
      ...combined.rows.map(row=>`| ${escape(row.name)} | ${escape(row.category)} | ${row.wallMs.toFixed(3)} |`),
      `| **Total** | | **${combined.balancedMs.toFixed(3)}** |`, '',
      '## Native detail (nested, not additional time)', '',
      ...native.flatMap(item=>[
        `Factory calls: ${item.factoryMs?.toFixed(3) ?? 'unavailable'} ms; constructors within them: ${item.constructorMs?.toFixed(3) ?? 'unavailable'} ms.`, '',
        item.warning, '',
      ]),
      ...(nativeBatches?.rows ? ['## Native read by payload service (nested)', '',
        nativeBatches.basis, '', '| Payload services | Batches | Read ms | Slowest batch ms |', '|---|---:|---:|---:|',
        ...nativeBatches.rows.map(row=>`| ${escape(row.services.join(', '))} | ${row.batches} | ${row.readMs.toFixed(3)} | ${row.maxBatchMs.toFixed(3)} |`), '',
        `Native clock reads and batch-boundary bookkeeping: ${nativeBatches.boundaryAndClockMs.toFixed(3)} ms.`, ''] : []),
      '## Coverage limits', '',
      ...combined.warnings.map(warning=>`- ${warning}`),
      '- The engine reader remains a grouped native call. Its internals and other plugins are not individually attributed by these timers.',
      '- Parallel worker durations are not additive elapsed time. See the worker timelines in the JSON output.',
      '- Trace capture has overhead. Establish speed improvements with separate ordinary runs and saved-place comparisons.',
      ...(combined.unaccountedMs > 0 ? ['- Uninstrumented intervals remain visible in the JSON; this report does not claim fully detailed coverage.'] : []),
      '',
    ];
    fs.writeFileSync(prefix + '.report.md', report.join('\n'));
  }
  console.log(JSON.stringify({spans: events.length, wallMs: accounting?.wallMs,
    unaccountedMs: combined?.unaccountedMs ?? accounting?.unaccountedMs,
    top: combined?.rows.slice(0,8) ?? accounting?.rows.slice(0,8) ?? rows.slice(0,8), native}));
}
