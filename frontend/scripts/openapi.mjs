import { execFileSync } from 'node:child_process'
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { fileURLToPath } from 'node:url'

const mode = process.argv[2]

if (mode !== 'write' && mode !== 'check') {
  console.error('Usage: node scripts/openapi.mjs <write|check>')
  process.exit(1)
}

const frontend = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const cli = resolve(frontend, 'node_modules', 'openapi-typescript', 'bin', 'cli.js')
const schema = resolve(dirname(fileURLToPath(import.meta.url)), '../../docs/api/openapi.json')
const committed = resolve(frontend, 'src/api/generated.ts')

if (mode === 'write') {
  mkdirSync(dirname(committed), { recursive: true })
  execFileSync(process.execPath, [cli, schema, '-o', committed], { stdio: 'inherit' })
} else {
  const temporaryDirectory = mkdtempSync(join(tmpdir(), 'tessera-openapi-'))
  const generated = join(temporaryDirectory, 'generated.ts')

  try {
    execFileSync(process.execPath, [cli, schema, '-o', generated], { stdio: 'inherit' })

    if (!existsSync(committed) || !readFileSync(generated).equals(readFileSync(committed))) {
      console.error('OpenAPI contract is out of date. Run npm run generate:api.')
      process.exitCode = 1
    }
  } finally {
    rmSync(temporaryDirectory, { recursive: true, force: true })
  }
}
