import type { DetectorSpan } from '../api/detector'

export type DetectionLayer = 'deterministic' | 'ner' | 'other'

export type HighlightedSegment =
  | { kind: 'text'; text: string }
  | { kind: 'detection'; text: string; span: DetectorSpan; layer: DetectionLayer }

export class InvalidDetectionResponseError extends Error {
  constructor() {
    super('Detector returned invalid span offsets.')
    this.name = 'InvalidDetectionResponseError'
  }
}

export function classifyRecognizer(recognizer: string): DetectionLayer {
  if (recognizer.startsWith('catalog:')) {
    return 'deterministic'
  }
  if (recognizer.startsWith('ner:')) {
    return 'ner'
  }
  return 'other'
}

export function buildHighlightedSegments(
  text: string,
  spans: DetectorSpan[],
): HighlightedSegment[] {
  const codePoints = Array.from(text)
  const orderedSpans = [...spans].sort((left, right) => (
    left.start - right.start || left.end - right.end
  ))
  const segments: HighlightedSegment[] = []
  let cursor = 0

  for (const span of orderedSpans) {
    if (!Number.isInteger(span.start)
      || !Number.isInteger(span.end)
      || span.start < 0
      || span.end <= span.start
      || span.end > codePoints.length
      || span.start < cursor) {
      throw new InvalidDetectionResponseError()
    }

    if (span.start > cursor) {
      segments.push({ kind: 'text', text: codePoints.slice(cursor, span.start).join('') })
    }
    segments.push({
      kind: 'detection',
      text: codePoints.slice(span.start, span.end).join(''),
      span,
      layer: classifyRecognizer(span.recognizer),
    })
    cursor = span.end
  }

  if (cursor < codePoints.length || segments.length === 0) {
    segments.push({ kind: 'text', text: codePoints.slice(cursor).join('') })
  }

  return segments
}
