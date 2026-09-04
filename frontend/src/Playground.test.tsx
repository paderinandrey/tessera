import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { QueryClient } from '@tanstack/react-query'
import userEvent from '@testing-library/user-event'
import { delay, http, HttpResponse } from 'msw'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { App } from './App'
import { server } from './test/server'

const fullHealth = { status: 'ok' as const, ner: true, ner_off_reason: null }
const personResponse = {
  spans: [{
    entity_type: 'PERSON', start: 0, end: 13, confidence: 0.91,
    recognizer: 'ner:gliner', tier: 2, boosted: false,
  }],
  layers_run: ['deterministic', 'ner'],
  version: 'test-version',
}
const emptyResponse = {
  spans: [],
  layers_run: ['deterministic', 'ner'],
  version: 'test-version',
}
const deterministicOnlyResponse = {
  spans: [],
  layers_run: ['deterministic'],
  version: 'test-version',
}

function renderApp(queryClient?: QueryClient) {
  return render(<App queryClient={queryClient} />)
}

describe('local detector playground', () => {
  beforeEach(() => {
    server.use(
      http.get('/api/health', () => HttpResponse.json(fullHealth)),
      http.post('/api/detect', () => HttpResponse.json(emptyResponse)),
    )
  })

  afterEach(() => {
    cleanup()
    vi.restoreAllMocks()
  })

  it('analyzes a non-empty snapshot while omitting layers', async () => {
    let received: unknown
    server.use(
      http.post('/api/detect', async ({ request }) => {
        received = await request.json()
        return HttpResponse.json(personResponse)
      }),
    )
    const user = userEvent.setup()

    renderApp()
    await user.type(screen.getByLabelText(/Text to inspect/i), 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByText('Model detected')).toBeVisible()
    expect(received).toEqual({ text: 'Martina Weber' })
  })

  it('keeps Analyze disabled for whitespace-only text', async () => {
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), '   ')

    expect(screen.getByRole('button', { name: /Analyze text/i })).toBeDisabled()
  })

  it('allows analysis when the health check is unavailable', async () => {
    server.use(http.get('/api/health', () => HttpResponse.json(
      { detail: 'The health check is unavailable.' }, { status: 503 },
    )))
    const user = userEvent.setup()
    renderApp()

    await screen.findByText('Detector unavailable.')
    await user.type(screen.getByLabelText(/Text to inspect/i), 'Martina')

    expect(screen.getByRole('button', { name: /Analyze text/i })).toBeEnabled()
  })

  it('disables only duplicate analysis while a request is pending', async () => {
    server.use(http.post('/api/detect', async () => {
      await delay(100)
      return HttpResponse.json(emptyResponse)
    }))
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), 'Martina')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(screen.getByRole('button', { name: /Analyze text/i })).toBeDisabled()
    expect(screen.getByRole('button', { name: /Clear/i })).toBeEnabled()
    expect(screen.getByLabelText(/Text to inspect/i)).toBeEnabled()
  })

  it('renders the submitted snapshot when input changes during a response', async () => {
    server.use(http.post('/api/detect', async () => {
      await delay(100)
      return HttpResponse.json(personResponse)
    }))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await user.clear(input)
    await user.type(input, 'Changed input')

    expect(await screen.findByRole('mark', { name: /Martina Weber.*Model detected/i })).toBeVisible()
    expect(input).toHaveValue('Changed input')
  })

  it('clears stale output when another analysis begins', async () => {
    let calls = 0
    server.use(http.post('/api/detect', async () => {
      calls += 1
      if (calls === 1) {
        return HttpResponse.json(personResponse)
      }
      await delay(100)
      return HttpResponse.json(emptyResponse)
    }))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByText('Model detected')
    await user.clear(input)
    await user.type(input, 'No entities')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(screen.queryByText('Model detected')).not.toBeInTheDocument()
  })

  it('clears input and result when Clear is pressed', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json(personResponse)))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByText('Model detected')
    await user.click(screen.getByRole('button', { name: /Clear/i }))

    expect(input).toHaveValue('')
    expect(screen.queryByText('Model detected')).not.toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('does not retain cleared or previous submitted text in the query client', async () => {
    let calls = 0
    server.use(http.post('/api/detect', () => {
      calls += 1
      return HttpResponse.json(calls === 1 ? personResponse : emptyResponse)
    }))
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
    })
    const user = userEvent.setup()
    renderApp(queryClient)

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'First private submission')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByText('Model detected')
    expect(JSON.stringify({
      queries: queryClient.getQueryCache().getAll(),
      mutations: queryClient.getMutationCache().getAll(),
    })).not.toContain('First private submission')
    await user.click(screen.getByRole('button', { name: /Clear/i }))

    await waitFor(() => expect(JSON.stringify({
      queries: queryClient.getQueryCache().getAll(),
      mutations: queryClient.getMutationCache().getAll(),
    })).not.toContain('First private submission'))

    await user.type(input, 'Second private submission')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByText(/No entities were detected/i)

    expect(JSON.stringify({
      queries: queryClient.getQueryCache().getAll(),
      mutations: queryClient.getMutationCache().getAll(),
    })).not.toContain('First private submission')
  })

  it('keeps all result presentation hidden when detector offsets are invalid', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json({
      ...personResponse,
      spans: [{ ...personResponse.spans[0], end: 99 }],
    })))
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByRole('alert')).toHaveTextContent('Detector returned invalid span offsets.')
    expect(screen.queryByRole('table')).not.toBeInTheDocument()
    expect(screen.queryByText('Model detected')).not.toBeInTheDocument()
  })

  it('explains the idle result state before an analysis', () => {
    renderApp()

    expect(screen.getByText('Detections will appear here after you analyze text.')).toBeVisible()
  })

  it('does not render detection details for a successful zero-span response', async () => {
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), 'No detected entities')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByText(/No entities were detected/i)).toBeVisible()
    expect(screen.queryByRole('table')).not.toBeInTheDocument()
  })

  it('shows the result-specific limited scan when health still reports full detection', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json(deterministicOnlyResponse)))
    const user = userEvent.setup()
    renderApp()

    await screen.findByText('Full detection active.')
    await user.type(screen.getByLabelText(/Text to inspect/i), 'No detected entities')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByText('Limited scan: deterministic only — NER did not run.')).toBeVisible()
    expect(screen.getByText('Full detection active.')).toBeVisible()
  })

  it('shows the result-specific full scan when deterministic and NER layers ran', async () => {
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), 'No detected entities')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByText('Full scan: deterministic + NER')).toBeVisible()
  })

  it('clears an analysis error when Clear is pressed', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json(
      { detail: 'The detector rejected this text.' }, { status: 422 },
    )))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByRole('alert')
    await user.click(screen.getByRole('button', { name: /Clear/i }))

    expect(input).toHaveValue('')
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('does not restore a delayed success after Clear', async () => {
    server.use(http.post('/api/detect', async () => {
      await delay(100)
      return HttpResponse.json(personResponse)
    }))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await user.click(screen.getByRole('button', { name: /Clear/i }))
    await delay(150)

    expect(input).toHaveValue('')
    expect(screen.queryByText('Model detected')).not.toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('does not restore a delayed error after Clear', async () => {
    server.use(http.post('/api/detect', async () => {
      await delay(100)
      return HttpResponse.json({ detail: 'The detector rejected this text.' }, { status: 422 })
    }))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await user.click(screen.getByRole('button', { name: /Clear/i }))
    await delay(150)

    expect(input).toHaveValue('')
    expect(screen.queryByText('Model detected')).not.toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('cancels the cleared request before accepting a new analysis', async () => {
    let requests = 0
    let activeRequests = 0
    let maximumActiveRequests = 0
    server.use(http.post('/api/detect', async ({ request }) => {
      requests += 1
      activeRequests += 1
      maximumActiveRequests = Math.max(maximumActiveRequests, activeRequests)

      if (requests === 1) {
        await Promise.race([
          delay(200),
          new Promise<void>((resolve) => {
            if (request.signal.aborted) {
              resolve()
              return
            }
            request.signal.addEventListener('abort', () => resolve(), { once: true })
          }),
        ])
        activeRequests -= 1
        return HttpResponse.json(personResponse)
      }

      activeRequests -= 1
      return HttpResponse.json(emptyResponse)
    }))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await user.click(screen.getByRole('button', { name: /Clear/i }))
    await user.type(input, 'New text')
    await waitFor(() => expect(screen.getByRole('button', { name: /Analyze text/i })).toBeEnabled())
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByText(/No entities were detected/i)
    await delay(250)

    expect(maximumActiveRequests).toBe(1)
    expect(screen.getByText(/No entities were detected/i)).toBeVisible()
    expect(screen.queryByText('Model detected')).not.toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('shows structured error detail without clearing input', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json(
      { detail: 'The detector rejected this text.' }, { status: 422 },
    )))
    const user = userEvent.setup()
    renderApp()

    const input = screen.getByLabelText(/Text to inspect/i)
    await user.type(input, 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByRole('alert')).toHaveTextContent('The detector rejected this text.')
    expect(input).toHaveValue('Martina Weber')
  })

  it('shows fixed safe copy for malformed error JSON', async () => {
    server.use(http.post('/api/detect', () => new HttpResponse('not json', { status: 500 })))
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))

    expect(await screen.findByRole('alert')).toHaveTextContent('Detector request failed.')
  })

  it('does not write text to the console during analysis errors', async () => {
    const log = vi.spyOn(console, 'log')
    const error = vi.spyOn(console, 'error')
    server.use(http.post('/api/detect', () => HttpResponse.json(
      { detail: 'The detector rejected this text.' }, { status: 422 },
    )))
    const user = userEvent.setup()
    renderApp()

    await user.type(screen.getByLabelText(/Text to inspect/i), 'Martina Weber')
    await user.click(screen.getByRole('button', { name: /Analyze text/i }))
    await screen.findByRole('alert')
    await waitFor(() => expect(log).not.toHaveBeenCalled())

    expect(error).not.toHaveBeenCalled()
  })
})
