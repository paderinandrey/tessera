import { describe, expect, it } from 'vitest'

import type { DetectorSpan } from '../api/detector'
import {
  buildHighlightedSegments,
  classifyRecognizer,
  InvalidDetectionResponseError,
} from './detections'

function span(overrides: Partial<DetectorSpan> = {}): DetectorSpan {
  return {
    entity_type: 'PERSON',
    start: 0,
    end: 1,
    confidence: 0.91,
    recognizer: 'ner:gliner',
    tier: 2,
    boosted: false,
    ...overrides,
  }
}

describe('classifyRecognizer', () => {
  it('classifies catalog recognizers as deterministic', () => {
    expect(classifyRecognizer('catalog:credit_card')).toBe('deterministic')
  })

  it('classifies ner recognizers as ner', () => {
    expect(classifyRecognizer('ner:gliner')).toBe('ner')
  })

  it('classifies unknown recognizer prefixes as other', () => {
    expect(classifyRecognizer('custom:recognizer')).toBe('other')
  })
})

describe('buildHighlightedSegments', () => {
  it('uses Unicode code points rather than UTF-16 indexes', () => {
    const detected = span({ start: 2, end: 9, recognizer: 'ner:gliner' })

    expect(buildHighlightedSegments('🙂 Martina', [detected])).toEqual([
      { kind: 'text', text: '🙂 ' },
      { kind: 'detection', text: 'Martina', span: detected, layer: 'ner' },
    ])
  })

  it('preserves text before, between, and after ordered spans', () => {
    const segments = buildHighlightedSegments('Anna in Bern.', [
      span({ start: 0, end: 4 }),
      span({ entity_type: 'LOCATION', start: 8, end: 12 }),
    ])

    expect(segments.map((segment) => segment.text)).toEqual(['Anna', ' in ', 'Bern', '.'])
  })

  it('accepts unsorted spans and returns them in text order', () => {
    const later = span({ start: 5, end: 9 })
    const earlier = span({ start: 0, end: 4 })

    expect(buildHighlightedSegments('Anna Bern', [later, earlier]).map((segment) => segment.text))
      .toEqual(['Anna', ' ', 'Bern'])
  })

  it('returns the complete text when there are no spans', () => {
    expect(buildHighlightedSegments('No detections.', [])).toEqual([
      { kind: 'text', text: 'No detections.' },
    ])
  })

  it.each([{ start: -1, end: 2 }, { start: 2, end: 2 }, { start: 0, end: 99 }])(
    'rejects invalid bounds $start..$end',
    (bounds) => {
      const detected = span(bounds)

      expect(() => buildHighlightedSegments('abc', [detected]))
        .toThrow('Detector returned invalid span offsets.')
      expect(() => buildHighlightedSegments('abc', [detected]))
        .toThrow(InvalidDetectionResponseError)
    },
  )

  it('rejects overlapping spans', () => {
    expect(() => buildHighlightedSegments('abcdef', [
      span({ start: 0, end: 4 }),
      span({ start: 3, end: 6 }),
    ])).toThrow('Detector returned invalid span offsets.')
  })
})
