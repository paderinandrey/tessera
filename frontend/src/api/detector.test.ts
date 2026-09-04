import { http, HttpResponse } from 'msw'
import { describe, expect, it } from 'vitest'

import { detectText, getHealth } from './detector'
import { server } from '../test/server'

describe('detector API adapter', () => {
  it('returns a complete health response', async () => {
    server.use(http.get('/api/health', () => HttpResponse.json({
      status: 'ok', ner: false, ner_off_reason: 'weights unavailable',
    })))

    await expect(getHealth()).resolves.toEqual({
      status: 'ok', ner: false, ner_off_reason: 'weights unavailable',
    })
  })

  it('sends only text when all available layers should run', async () => {
    let received: unknown
    server.use(http.post('/api/detect', async ({ request }) => {
      received = await request.json()
      return HttpResponse.json({ spans: [], layers_run: ['deterministic'], version: 'v1' })
    }))

    await detectText({ text: 'CH9300762011623852957' })

    expect(received).toEqual({ text: 'CH9300762011623852957' })
  })

  it('uses a safe detail without including submitted text', async () => {
    server.use(http.post('/api/detect', () =>
      HttpResponse.json({ detail: 'detector unavailable' }, { status: 503 })))

    await expect(detectText({ text: 'Martina Weber' })).rejects.toMatchObject({
      message: 'detector unavailable',
    })
  })

  it('uses fixed copy for a non-JSON failure', async () => {
    server.use(http.post('/api/detect', () =>
      new HttpResponse('upstream gateway message', { status: 502 })))

    await expect(detectText({ text: 'secret' })).rejects.toMatchObject({
      message: 'Detector request failed.',
    })
  })

  it('rejects malformed success JSON with fixed copy', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json({ spans: 'wrong' })))

    await expect(detectText({ text: 'secret' })).rejects.toMatchObject({
      message: 'Detector returned an invalid response.',
    })
  })

  it('rejects malformed health JSON with fixed copy', async () => {
    server.use(http.get('/api/health', () => HttpResponse.json({ status: 'ok', ner: 'false' })))

    await expect(getHealth()).rejects.toMatchObject({
      message: 'Detector returned an invalid response.',
    })
  })

  it('returns complete span metadata including boosted', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json({
      spans: [{
        start: 0,
        end: 13,
        entity_type: 'PERSON',
        recognizer: 'ner',
        confidence: 0.98,
        tier: 2,
        boosted: true,
      }],
      layers_run: ['deterministic', 'ner'],
      version: 'weights-2026-08',
    })))

    await expect(detectText({ text: 'Martina Weber' })).resolves.toEqual({
      spans: [{
        start: 0,
        end: 13,
        entity_type: 'PERSON',
        recognizer: 'ner',
        confidence: 0.98,
        tier: 2,
        boosted: true,
      }],
      layers_run: ['deterministic', 'ner'],
      version: 'weights-2026-08',
    })
  })

  it('defaults an absent boosted property to false', async () => {
    server.use(http.post('/api/detect', () => HttpResponse.json({
      spans: [{
        start: 0,
        end: 13,
        entity_type: 'PERSON',
        recognizer: 'ner',
        confidence: 0.98,
        tier: 2,
      }],
      layers_run: ['ner'],
      version: 'weights-2026-08',
    })))

    await expect(detectText({ text: 'Martina Weber' })).resolves.toMatchObject({
      spans: [{ boosted: false }],
    })
  })

  it.each([0, 4])('rejects spans with tier %i outside the documented range', async (tier) => {
    server.use(http.post('/api/detect', () => HttpResponse.json({
      spans: [{
        start: 0,
        end: 7,
        entity_type: 'PERSON',
        recognizer: 'ner:gliner',
        confidence: 0.98,
        tier,
        boosted: false,
      }],
      layers_run: ['ner'],
      version: 'weights-2026-08',
    })))

    await expect(detectText({ text: 'Martina' })).rejects.toMatchObject({
      message: 'Detector returned an invalid response.',
    })
  })

  it('passes an AbortSignal through to fetch', async () => {
    const controller = new AbortController()
    controller.abort()

    await expect(detectText({ text: 'secret' }, controller.signal)).rejects.toMatchObject({
      name: 'AbortError',
    })
  })
})
