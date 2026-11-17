// Generates crates/beatseam-api/tests/data/{dto,client}_golden.json from the *TypeScript* packages/api (zod schemas and
// ApiClient), for tests/dto_golden.rs and tests/client_golden.rs to hold the Rust port to:
//
//   dto_golden:    for each schema, inputs zod accepts together with what zod makes of them (`parsed`), and inputs it
//                  rejects. The Rust DTO must deserialize the former, serialize back to an equivalent value, and
//                  refuse the latter.
//   client_golden: the exact request (method, url, headers, body) every ApiClient method sends, and how the client
//                  turns assorted error responses into error classes.
//
//   node --experimental-transform-types crates/beatseam-api/tools/gen-api-golden.mjs
//
// The TS sources use extensionless relative imports, so they are copied (with `.ts` added) into a scratch folder INSIDE
// packages/api, where Node can still find that package's node_modules/zod. The folder is removed afterwards.
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const apiSrc = join(here, '../../../packages/api/src');
const work = join(apiSrc, '..', '.golden-tmp');
rmSync(work, { recursive: true, force: true });
mkdirSync(work, { recursive: true });
for (const f of readdirSync(apiSrc)) {
  if (!f.endsWith('.ts') || f.endsWith('.test.ts')) continue;
  const text = readFileSync(join(apiSrc, f), 'utf8').replace(/from '(\.\/[A-Za-z]+)'/g, "from '$1.ts'");
  writeFileSync(join(work, f), text);
}
writeFileSync(join(work, 'package.json'), '{"type":"module"}');
const load = (f) => import(pathToFileURL(join(work, `${f}.ts`)).href);

try {
  const S = await load('schemas');
  const A = await load('auth');
  const L = await load('lastfm');
  const M = await load('mutations');
  const Y = await load('sync');
  const { ApiClient } = await load('client');

  // ------------------------------------------------------------------ sample payloads
  const track = (n, extra = {}) => ({ id: `k${n}`, rootId: 'Music', relativePath: `A/${n}.mp3`, sizeBytes: 1000 + n, lastModifiedMs: 1700000000000 + n, missing: false, title: `T${n}`, artists: ['A', 'B'], album: 'Al', durationSeconds: 200.5, contentHash: 'ab'.repeat(32), normalizationGain: 0.8, hasCover: true, hasLyrics: false, ...extra });
  const root = { id: 'Music', displayName: 'Music', kind: 'library' };
  const me = { id: 'u1', username: 'went', displayName: 'Went', role: 'ADMIN', enrolled: true, hasPassword: true, totpEnabled: false, passkeys: [{ id: 'p1', name: 'Phone', deviceType: 'singleDevice', backedUp: false, createdAtMs: 1700000000000, lastUsedAtMs: null }], recoveryCodesRemaining: 8, authenticatedAtMs: 1700000001000, freshUntilMs: null, serverTimeMs: 1700000002000, via: 'session' };
  const jobs = { pending: 1, retrying: 0, running: 2, done: 30, skipped: 4 };
  const userSummary = { id: 'u2', username: 'amy', displayName: 'Amy', role: 'USER', disabled: false, enrolled: false, createdAtMs: 1700000000000 };
  const userPlaylist = { id: 'pl_12345678', name: 'Mix', trackIds: ['k1', 'k2'], updatedAtMs: 1700000000500 };
  const playbackState = { deviceId: 'd1', state: { trackId: 'k1', positionSeconds: 12.5, nested: { a: [1, 2, null] } }, updatedAtMs: 1700000000600 };
  const setting = { key: 'theme', value: 'dark', updatedAtMs: 1700000000700 };
  const pl = { id: 'pl1', rootId: 'Music', name: 'Shared', trackIds: ['k1', 'k1', 'k2'] };
  const lyrics = { synced: true, tags: { ti: 'T', ar: 'A' }, lines: [{ timeSeconds: 1.5, text: 'a', translation: 'x' }, { timeSeconds: 2, text: 'b', translation: null }, { timeSeconds: null, text: 'c' }] };

  const dto = [];
  // An invalid sample can be wrapped as skip(reason, value): zod rejects it, but the Rust DTOs deliberately do not (they carry
  // shapes, not the value constraints, and treat a missing nullable key as null), so the Rust test does not require a refusal.
  const skip = (reason, value) => ({ __skip: reason, value });
  const add = (schema, name, valid, invalid = []) => dto.push({ name, valid: valid.map((input) => ({ input, parsed: schema.parse(input) })), invalid: invalid.map((item) => { const wrapped = item && typeof item === 'object' && '__skip' in item; const input = wrapped ? item.value : item; if (schema.safeParse(input).success) throw new Error(`${name}: expected zod to reject ${JSON.stringify(input)}`); return wrapped ? { input, skip: item.__skip } : { input }; }) });

  add(S.rootSchema, 'root', [root, { ...root, kind: 'lyrics' }, { ...root, extra: 1 }], [{ ...root, kind: 'other' }, { id: 'a', displayName: 'b' }]);
  add(S.trackSchema, 'track', [track(1), track(2, { lastModifiedMs: 1700000000123.456, title: null, album: null, durationSeconds: null, contentHash: null, normalizationGain: null, artists: [], missing: true, unknownField: 'ignored' })], [{ ...track(1), hasCover: undefined }, { ...track(1), sizeBytes: '12' }, { ...track(1), artists: 'A' }]);
  add(S.playlistSchema, 'playlist', [pl], [{ ...pl, trackIds: [1] }]);
  add(S.lyricsSchema, 'lyrics', [lyrics, { synced: false, tags: {}, lines: [] }], [skip('nullable-missing', { synced: true, tags: {}, lines: [{ text: 'x' }] })]);
  add(S.lyricsFileSchema, 'lyricsFile', [{ id: 'f', name: 'a.lrc', relativePath: 'a.lrc', rootId: 'L' }]);
  add(S.lyricsSourceSchema, 'lyricsSource', [{ assigned: null, candidates: [] }, { assigned: 'f', candidates: [{ id: 'f', name: 'a.lrc', relativePath: 'a.lrc', rootId: 'L' }] }]);
  add(S.lyricsAssignRequestSchema, 'lyricsAssignRequest', [{ lrcFileId: 'f' }, { lrcFileId: null }], [skip('constraint', { lrcFileId: '' }), skip('nullable-missing', {})]);
  add(S.lyricsReportRequestSchema, 'lyricsReportRequest', [{}, { reason: 'wrong song' }]);
  add(S.lyricsReportSchema, 'lyricsReport', [{ id: 'r1', trackId: 'k1', relativePath: 'A/1.mp3', reportedBy: 'went', reason: null, createdAt: '2026-11-17T10:00:00.000Z' }]);
  add(S.lyricsSaveRequestSchema, 'lyricsSaveRequest', [{ lines: [{ timeSeconds: 0, text: 'a' }, { timeSeconds: null, text: 'b' }] }], [skip('constraint', { lines: [{ timeSeconds: -1, text: 'a' }] })]);
  add(S.pageSchema(S.trackSchema), 'page', [{ items: [track(1)], nextCursor: 'c' }, { items: [], nextCursor: null }], [skip('nullable-missing', { items: [] })]);
  add(S.jobCountsSchema, 'jobCounts', [jobs]);
  add(S.scanStatusSchema, 'scanStatus', [{ running: true, last: null, jobs, workers: { workers: 2, succeeded: 5, failedAttempts: 1, skipped: 0, inFlight: 1 } }, { running: false, last: { startedAtMs: 1700000000000, finishedAtMs: 1700000009000, roots: 2, tracks: 100, queued: 3, jobs }, jobs, workers: { workers: 2, succeeded: 5, failedAttempts: 1, skipped: 0, inFlight: 0 } }]);
  add(S.scanJobSchema, 'scanJob', [{ trackId: 'k1', relativePath: null, state: 'SKIPPED', attempts: 3, lastError: 'boom', lastErrorStage: 'metadata', skippedAtMs: 1700000000000, updatedAtMs: 1700000001000 }, { trackId: 'k1', relativePath: 'A/1.mp3', state: 'PENDING', attempts: 0, lastError: null, lastErrorStage: null, skippedAtMs: null, updatedAtMs: 1 }], [{ trackId: 'k1', relativePath: null, state: 'pending', attempts: 0, lastError: null, lastErrorStage: null, skippedAtMs: null, updatedAtMs: 1 }]);
  add(S.scanFailureSchema, 'scanFailure', [{ id: 7, trackId: 'k1', relativePath: null, stage: 'analysis', attempt: 2, error: 'x', willRetry: true, workerId: 'w1', atMs: 1700000000000 }]);

  add(A.authStatusSchema, 'authStatus', [{ setupRequired: true, setupTokenRequired: false }]);
  add(A.setupRequestSchema, 'setupRequest', [{ username: 'went', displayName: 'Went' }, { username: 'went', displayName: 'Went', setupToken: 't' }]);
  add(A.redeemInviteRequestSchema, 'redeemInviteRequest', [{ inviteToken: 'abc' }]);
  add(A.passwordLoginRequestSchema, 'passwordLoginRequest', [{ username: 'went', password: 'pw' }]);
  add(A.totpLoginRequestSchema, 'totpLoginRequest', [{ mfaToken: 'm', code: '123456' }, { mfaToken: 'm', recoveryCode: 'abcd-efgh' }]);
  add(A.setPasswordRequestSchema, 'setPasswordRequest', [{ password: 'a-long-enough-password' }]);
  add(A.totpCodeRequestSchema, 'totpCodeRequest', [{ code: '123456' }]);
  add(A.createUserRequestSchema, 'createUserRequest', [{ username: 'amy', displayName: 'Amy' }, { username: 'amy', displayName: 'Amy', role: 'ADMIN' }]);
  add(A.updateUserRequestSchema, 'updateUserRequest', [{}, { displayName: 'X', role: 'USER', disabled: true }]);
  add(A.passkeySummarySchema, 'passkeySummary', [me.passkeys[0]]);
  add(A.meSchema, 'me', [me, { ...me, role: 'USER', via: 'device', authenticatedAtMs: null, freshUntilMs: 1700000009000 }], [{ ...me, via: 'cookie' }, { ...me, role: 'ROOT' }]);
  add(A.userSummarySchema, 'userSummary', [userSummary]);
  add(A.inviteResultSchema, 'inviteResult', [{ inviteToken: 'tok', expiresAtMs: 1700000009000 }]);
  add(A.createUserResultSchema, 'createUserResult', [{ user: userSummary, invite: { inviteToken: 'tok', expiresAtMs: 1700000009000 } }]);
  add(A.sessionSummarySchema, 'sessionSummary', [{ id: 's1', createdAtMs: 1, lastSeenAtMs: 2, userAgent: null, ip: '127.0.0.1', current: true }]);
  add(A.loginResultSchema, 'loginResult', [{ status: 'ok', me }, { status: 'mfa_required', mfaToken: 'm' }], [{ status: 'nope' }, { status: 'ok' }]);
  add(A.totpSetupSchema, 'totpSetup', [{ secret: 'S', otpauthUri: 'otpauth://totp/x' }]);
  add(A.recoveryCodesSchema, 'recoveryCodes', [{ recoveryCodes: ['a', 'b'] }]);
  add(A.passkeyRegisterVerifyRequestSchema, 'passkeyRegisterVerifyRequest', [{ response: { id: 'x', rawId: 'y', response: { attestationObject: 'z' } } }, { response: { id: 'x' }, name: 'Phone' }]);
  add(A.passkeyRenameRequestSchema, 'passkeyRenameRequest', [{ name: 'New' }]);
  add(A.passkeyLoginVerifyRequestSchema, 'passkeyLoginVerifyRequest', [{ challengeId: 'c', response: { id: 'x' } }]);
  add(A.passkeyLoginOptionsSchema, 'passkeyLoginOptions', [{ challengeId: 'c', options: { challenge: 'abc', timeout: 60000, userVerification: 'preferred' } }]);
  add(A.deviceCodeRequestSchema, 'deviceCodeRequest', [{ deviceName: 'Pixel' }]);
  add(A.deviceCodeResultSchema, 'deviceCodeResult', [{ deviceCode: 'dc', userCode: 'ABCD-1234', verificationUri: 'https://x/device?code=ABCD-1234', expiresInSeconds: 600 }]);
  add(A.deviceWaitRequestSchema, 'deviceWaitRequest', [{ deviceCode: 'dc' }]);
  add(A.deviceWaitResultSchema, 'deviceWaitResult', [{ status: 'pending' }, { status: 'approved', token: 'tok', me }, { status: 'denied' }, { status: 'expired' }], [{ status: 'approved' }, { status: 'maybe' }]);
  add(A.deviceLookupSchema, 'deviceLookup', [{ deviceName: 'Pixel', requestedAtMs: 1700000000000 }]);
  add(A.deviceApproveRequestSchema, 'deviceApproveRequest', [{ userCode: 'ABCD-1234', approve: true }]);
  add(A.deviceTokenSummarySchema, 'deviceTokenSummary', [{ id: 't1', name: 'Pixel', createdAtMs: 1, lastUsedAtMs: null }]);

  add(L.lastFmStatusSchema, 'lastFmStatus', [{ connected: true, username: 'went' }, { connected: false, username: null }]);
  add(L.lastFmConnectResultSchema, 'lastFmConnectResult', [{ authUrl: 'https://www.last.fm/api/auth/?x' }]);
  add(L.lastFmNowPlayingRequestSchema, 'lastFmNowPlayingRequest', [{ artist: 'A', track: 'T' }, { artist: 'A', track: 'T', album: 'Al', durationSeconds: 200.5 }, { artist: 'A', track: 'T', album: null, durationSeconds: null }]);
  add(L.lastFmScrobblesRequestSchema, 'lastFmScrobblesRequest', [{ entries: [{ artist: 'A', track: 'T', timestamp: 1700000000, album: null, durationSeconds: 180 }] }], [skip('constraint', { entries: [] })]);

  const mutations = [
    { type: 'playlist.create', opId: 'op-00000001', id: 'pl_12345678', name: 'Mix', trackIds: ['k1'] },
    { type: 'playlist.create', opId: 'op-00000002', id: 'pl_12345678', name: 'Mix' },
    { type: 'playlist.rename', opId: 'op-00000003', id: 'pl_12345678', name: 'New' },
    { type: 'playlist.delete', opId: 'op-00000004', id: 'pl_12345678' },
    { type: 'playlist.addTracks', opId: 'op-00000005', id: 'pl_12345678', trackIds: ['k1', 'k2'] },
    { type: 'playlist.removeTracks', opId: 'op-00000006', id: 'pl_12345678', trackIds: ['k1'] },
    { type: 'playlist.setTracks', opId: 'op-00000007', id: 'pl_12345678', trackIds: [] },
    { type: 'playbackState.set', opId: 'op-00000008', deviceId: 'd1', state: { a: 1, b: [true, null] } },
    { type: 'setting.set', opId: 'op-00000009', key: 'theme', value: 'dark' },
    { type: 'setting.delete', opId: 'op-0000000a', key: 'theme' },
  ];
  add(M.mutationSchema, 'mutation', mutations, [{ type: 'playlist.explode', opId: 'op-00000001', id: 'x' }, skip('constraint', { type: 'setting.set', opId: 'short', key: 'k', value: 'v' })]);
  add(M.mutationsRequestSchema, 'mutationsRequest', [{ mutations: mutations.slice(0, 3) }]);
  add(M.mutationResultSchema, 'mutationResult', [{ opId: 'op-1', status: 'applied' }, { opId: 'op-1', status: 'rejected', error: 'nope' }, { opId: 'op-1', status: 'duplicate' }], [{ opId: 'o', status: 'later' }]);
  add(M.mutationsResponseSchema, 'mutationsResponse', [{ results: [{ opId: 'op-1', status: 'applied' }] }]);

  const changes = [
    { entity: 'track', id: 'k1', track: track(1) }, { entity: 'track', id: 'k2', track: null },
    { entity: 'playlist', id: 'pl1', playlist: pl }, { entity: 'playlist', id: 'pl2', playlist: null },
    { entity: 'userPlaylist', id: 'pl_12345678', userPlaylist: userPlaylist }, { entity: 'userPlaylist', id: 'x', userPlaylist: null },
    { entity: 'playbackState', id: 'd1', playbackState: playbackState }, { entity: 'playbackState', id: 'd2', playbackState: null },
    { entity: 'setting', id: 'theme', setting: setting }, { entity: 'setting', id: 'gone', setting: null },
  ];
  add(Y.userPlaylistSchema, 'userPlaylist', [userPlaylist]);
  add(Y.playbackStateSchema, 'playbackState', [playbackState]);
  add(Y.userSettingSchema, 'userSetting', [setting]);
  add(Y.syncChangeSchema, 'syncChange', changes, [{ entity: 'album', id: 'x', album: null }, skip('nullable-missing', { entity: 'track', id: 'x' })]);
  add(Y.syncChangesResponseSchema, 'syncChangesResponse', [{ changes, cursor: '42', hasMore: true, roots: [root], totals: { tracks: 10, playlists: 2 } }, { changes: [], cursor: '0', hasMore: false, roots: [], totals: { tracks: 0, playlists: 0 } }]);
  add(Y.syncSnapshotResponseSchema, 'syncSnapshotResponse', [{ entities: changes.slice(0, 4), next: 'n1', cursor: '7', roots: [root] }, { entities: [], next: null, cursor: '7', roots: [] }]);
  add(Y.resyncRequiredSchema, 'resyncRequired', [{ code: 'resync_required', message: 'too old' }], [{ code: 'other', message: 'x' }]);

  // ------------------------------------------------------------------ the client
  const BASE = 'https://music.example.com';
  const answerFor = (name) => ({ authStatus: { setupRequired: false, setupTokenRequired: false }, me, deviceCode: { deviceCode: 'dc', userCode: 'U', verificationUri: 'https://x', expiresInSeconds: 600 }, deviceWait: { status: 'pending' }, lastFmStatus: { connected: false, username: null }, lastFmConnect: { authUrl: 'https://l' }, listTracks: { items: [track(1)], nextCursor: null }, syncChanges: { changes: [], cursor: '1', hasMore: false, roots: [], totals: { tracks: 0, playlists: 0 } }, syncSnapshot: { entities: [], next: null, cursor: '1', roots: [] }, mutations: { results: [{ opId: 'op-1', status: 'applied' }] }, lyrics, lyricsReports: [], lyricsSource: { assigned: null, candidates: [] } }[name] ?? {});
  const calls = [
    ['authStatus', [], (c) => c.authStatus()],
    ['me', [], (c) => c.me()],
    ['deviceCode', ['Pixel 8 é'], (c) => c.deviceCode('Pixel 8 é')],
    ['deviceWait', ['dc/1'], (c) => c.deviceWait('dc/1')],
    ['logout', [], (c) => c.logout()],
    ['signOutThisDevice', [], (c) => c.signOutThisDevice()],
    ['lastFmStatus', [], (c) => c.lastFmStatus()],
    ['lastFmConnect', [], (c) => c.lastFmConnect()],
    ['lastFmComplete', [], (c) => c.lastFmComplete()],
    ['lastFmDisconnect', [], (c) => c.lastFmDisconnect()],
    ['lastFmNowPlaying', [{ artist: 'A', track: 'T', album: null, durationSeconds: 200.5 }], (c) => c.lastFmNowPlaying({ artist: 'A', track: 'T', album: null, durationSeconds: 200.5 })],
    ['lastFmNowPlaying', [{ artist: 'A', track: 'T' }], (c) => c.lastFmNowPlaying({ artist: 'A', track: 'T' })],
    ['lastFmScrobble', [[{ artist: 'A', track: 'T', timestamp: 1700000000, album: 'Al', durationSeconds: null }]], (c) => c.lastFmScrobble([{ artist: 'A', track: 'T', timestamp: 1700000000, album: 'Al', durationSeconds: null }])],
    ['listTracks', [{}], (c) => c.listTracks({})],
    ['listTracks', [{ rootId: 'Music', q: 'a b&c é', cursor: 'x/y', limit: 50 }], (c) => c.listTracks({ rootId: 'Music', q: 'a b&c é', cursor: 'x/y', limit: 50 })],
    ['listTracks', [{ limit: 0 }], (c) => c.listTracks({ limit: 0 })],
    ['listTracks', [{ q: '~!*()' }], (c) => c.listTracks({ q: '~!*()' })],
    ['syncChanges', ['12', null], (c) => c.syncChanges('12')],
    ['syncChanges', ['1 2/3&x', 100], (c) => c.syncChanges('1 2/3&x', 100)],
    ['syncChanges', ['5', 0], (c) => c.syncChanges('5', 0)],
    ['syncSnapshot', [null, null], (c) => c.syncSnapshot()],
    ['syncSnapshot', ['abc def/é', 10], (c) => c.syncSnapshot('abc def/é', 10)],
    ['syncSnapshot', ['', 0], (c) => c.syncSnapshot('', 0)],
    ['mutations', [[mutations[0], mutations[2], mutations[7]]], (c) => c.mutations([mutations[0], mutations[2], mutations[7]])],
    ['lyrics', ['a2V5'], (c) => c.lyrics('a2V5')],
    ['reportLyrics', ['a2V5', 'bad time'], (c) => c.reportLyrics('a2V5', 'bad time')],
    ['reportLyrics', ['a2V5', null], (c) => c.reportLyrics('a2V5')],
    ['reportLyrics', ['a2V5', ''], (c) => c.reportLyrics('a2V5', '')],
    ['lyricsReports', [], (c) => c.lyricsReports()],
    ['resolveLyricsReport', ['id/1 é'], (c) => c.resolveLyricsReport('id/1 é')],
    ['lyricsSource', ['a2V5'], (c) => c.lyricsSource('a2V5')],
    ['assignLyrics', ['a2V5', 'F:1'], (c) => c.assignLyrics('a2V5', 'F:1')],
    ['assignLyrics', ['a2V5', null], (c) => c.assignLyrics('a2V5', null)],
    ['saveLyrics', ['a2V5', [{ timeSeconds: 1.5, text: 'a' }, { timeSeconds: null, text: 'b' }]], (c) => c.saveLyrics('a2V5', [{ timeSeconds: 1.5, text: 'a' }, { timeSeconds: null, text: 'b' }])],
    ['fetchBytes', ['/api/v1/tracks/a2V5/stream', null], (c) => c.fetchBytes('/api/v1/tracks/a2V5/stream')],
    ['fetchBytes', ['/api/v1/tracks/a2V5/stream', [100, 199]], (c) => c.fetchBytes('/api/v1/tracks/a2V5/stream', { range: { start: 100, end: 199 } })],
    ['fetchFile', ['/api/v1/tracks/a2V5/cover'], (c) => c.fetchFile('/api/v1/tracks/a2V5/cover')],
    ['fetchText', ['/api/v1/tracks/a2V5/lyrics.lrc'], (c) => c.fetchText('/api/v1/tracks/a2V5/lyrics.lrc')],
  ];
  const requests = [];
  for (const withToken of [true, false]) {
    for (const [name, args, run] of calls) {
      const sent = [];
      const fetchStub = async (url, init) => {
        sent.push({ method: init?.method ?? 'GET', url, headers: Object.entries(init?.headers ?? {}), body: init?.body ?? null });
        const json = name.startsWith('fetch') ? null : answerFor(name === 'deviceWait' ? 'deviceWait' : name);
        return new Response(json === null ? 'bytes' : JSON.stringify(json), { status: 200, headers: { 'content-type': name === 'fetchFile' ? 'image/webp' : 'application/json' } });
      };
      const client = new ApiClient({ baseUrl: `${BASE}/`, getToken: withToken ? () => 'tok-1' : undefined, fetch: fetchStub });
      let result = 'ok';
      try { await run(client); } catch (err) { result = `error: ${err?.message}`; }
      requests.push({ withToken, call: name, args, sent, result });
    }
  }
  const pathHelpers = { streamPath: new ApiClient({ baseUrl: BASE }).streamPath('KEY'), coverPath: new ApiClient({ baseUrl: BASE }).coverPath('KEY'), streamPathForFileId: new ApiClient({ baseUrl: BASE }).streamPathForFileId('Music:Album/é.mp3'), url: new ApiClient({ baseUrl: `${BASE}//` }).url('/x') };

  // ---- error mapping
  const errorCases = [];
  const callers = { me: (c) => c.me(), lyrics: (c) => c.lyrics('k'), fetchBytes: (c) => c.fetchBytes('/x') };
  const tryCase = async (label, call, spec) => {
    const client = new ApiClient({
      baseUrl: BASE,
      fetch: async () => {
        if (spec === 'network') throw new TypeError('fetch failed');
        return new Response(spec.body, { status: spec.status, statusText: spec.statusText ?? '', headers: { 'content-type': 'application/json', ...(spec.headers ?? {}) } });
      },
    });
    let out;
    try { const v = await callers[call](client); out = { kind: 'ok', value: v === null ? null : 'value' }; }
    catch (err) { out = { kind: err?.constructor?.name ?? 'unknown', status: err?.status ?? null, code: err?.code ?? null, message: String(err?.message), problem: err?.problem ?? null }; }
    errorCases.push({ label, call, spec, ...out });
  };
  const r = (status, body, extra = {}) => ({ status, body, ...extra });
  await tryCase('401 message string', 'me', r(401, JSON.stringify({ message: 'Sign in required', code: 'unauthorized' }), { statusText: 'Unauthorized' }));
  await tryCase('400 message array', 'me', r(400, JSON.stringify({ message: ['a is bad', 'b is bad'] }), { statusText: 'Bad Request' }));
  await tryCase('502 no json, status text', 'me', r(502, '<html>', { statusText: 'Bad Gateway' }));
  await tryCase('502 no json, no status text', 'me', r(502, '<html>'));
  await tryCase('500 empty body', 'me', r(500, ''));
  await tryCase('410 resync_required', 'me', r(410, JSON.stringify({ code: 'resync_required', message: 'too old' })));
  await tryCase('410 other code', 'me', r(410, JSON.stringify({ code: 'gone', message: 'gone' })));
  await tryCase('409 code only', 'me', r(409, JSON.stringify({ code: 'not_authorized' })));
  await tryCase('426 server newer', 'me', r(426, JSON.stringify({ code: 'api_version_unsupported', message: 'x' }), { headers: { 'X-Beatseam-Api-Versions': '2' } }));
  await tryCase('200 but server newer', 'me', r(200, JSON.stringify(me), { headers: { 'x-beatseam-api-versions': '2,3' } }));
  await tryCase('200, server lists only an unusable 0', 'me', r(200, JSON.stringify(me), { headers: { 'X-Beatseam-Api-Versions': '0' } }));
  await tryCase('200 wrong shape', 'me', r(200, JSON.stringify({ nope: true })));
  await tryCase('200 not json', 'me', r(200, 'not json'));
  await tryCase('lyrics 404 is null', 'lyrics', r(404, JSON.stringify({ message: 'no lyrics' })));
  await tryCase('lyrics 500 is an error', 'lyrics', r(500, JSON.stringify({ message: 'oops' })));
  await tryCase('fetchBytes 404', 'fetchBytes', r(404, 'nope', { statusText: 'Not Found' }));
  await tryCase('fetchBytes 500 no text', 'fetchBytes', r(500, 'nope'));
  await tryCase('network failure', 'me', 'network');

  mkdirSync(join(here, '../tests/data'), { recursive: true });
  writeFileSync(join(here, '../tests/data/dto_golden.json'), JSON.stringify(dto));
  writeFileSync(join(here, '../tests/data/client_golden.json'), JSON.stringify({ requests, pathHelpers, errorCases }));
  console.log(`dto: ${dto.length} schemas, ${dto.reduce((n, d) => n + d.valid.length, 0)} valid / ${dto.reduce((n, d) => n + d.invalid.length, 0)} invalid; client: ${requests.length} requests, ${errorCases.length} error cases`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
