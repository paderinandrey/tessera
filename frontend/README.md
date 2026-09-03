# Tessera frontend playground

The frontend playground is a local UI for inspecting what the Tessera detector
would find in pasted text. Run the detector in one terminal and the frontend
in another; run these commands from the repository root:

Use Node.js `^22.22.2`, `^24.15.0`, or `>=26.0.0`; the frontend dependency
lock requires one of these supported ranges.

```bash
npm --prefix frontend install
uv run --project detector --group serve tessera serve
npm --prefix frontend run dev
```

The detector listens on `127.0.0.1:8000` by default. The Vite development
server proxies `/api` requests to that address. To use another locally running
detector, set `TESSERA_DETECTOR_URL` before starting the frontend:

```bash
TESSERA_DETECTOR_URL=http://127.0.0.1:9000 npm --prefix frontend run dev
```

## NER and privacy boundary

NER weights are optional for startup. Without them, the detector runs its
deterministic layer only; `ner: false` is an honest, deterministic-only state
reported by detector health. To enable the full layer, install the `ner` and
`serve` dependency groups and download the weights with:

```bash
make model
```

The model download is optional and large. The detector can start and serve
deterministic results while weights are absent; restart it after installing
weights so the NER layer is loaded.

Pasted text is sent to the locally configured detector for inspection and is
not persisted by the frontend. Keep the detector local and do not point
`TESSERA_DETECTOR_URL` at an untrusted remote detector: that would send pasted
personal data outside the intended local privacy boundary.

The root `docker-compose.yml` does not publish detector port `8000`; only the
gateway is exposed to the host. The frontend playground therefore uses the
direct local detector command above (or another explicitly local detector),
not the Compose service from outside the Compose network. This playground has
no production deployment configuration and is not a production service.

## Quality checks

Run the complete frontend quality gate from the repository root:

```bash
cd frontend
npm run check:api
npm run typecheck
npm run lint
npm run test:run
npx playwright install chromium
npm run test:e2e
npm run build
```

Every command must exit 0. Test output should contain no React `act` warnings,
unhandled requests, or browser console errors.

Before committing frontend-only work, prove protected backend and contract
paths are untouched:

```bash
git diff --exit-code HEAD -- gateway detector docs/api/openapi.json docker-compose.yml
git status --short
```

For final verification, run all checks in one chain and inspect the branch:

```bash
cd frontend
npm run check:api && npm run typecheck && npm run lint && npm run test:run && npx playwright install chromium && npm run test:e2e && npm run build
cd ..
git status --short
git log --oneline --decorate -8
```
