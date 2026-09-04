import type { DetectorSpan } from '../api/detector'

export type DetectionLayer = 'checksum' | 'pattern' | 'ner' | 'other'

export type HighlightedSegment =
  | { kind: 'text'; text: string }
  | { kind: 'detection'; text: string; span: DetectorSpan; layer: DetectionLayer }

export class InvalidDetectionResponseError extends Error {
  constructor() {
    super('Detector returned invalid span offsets.')
    this.name = 'InvalidDetectionResponseError'
  }
}

/**
 * Which kind of evidence a span carries.
 *
 * **Two catalog rules carry no checksum**, and calling every `catalog:` match
 * "checksum verified" told the user something the detector never claimed: an
 * e-mail has no checksum to verify, and the German Steuernummer's validator is
 * structural. Both say so in `identifiers.yaml` beside the rules themselves.
 *
 * The distinction is already in the response rather than in knowledge this
 * client would have to keep in step with a catalog it does not ship: a rule
 * without a checksum stays below 1.0 on purpose, and `resolution.py` separates
 * them with exactly this predicate — `catalog:` and a confidence of 1.0. Asking
 * the same question the detector asks is how the answer stays true when the
 * catalog grows.
 */
export function classifyRecognizer(recognizer: string, confidence: number): DetectionLayer {
  if (recognizer.startsWith('catalog:')) {
    return confidence >= 1 ? 'checksum' : 'pattern'
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
      layer: classifyRecognizer(span.recognizer, span.confidence),
    })
    cursor = span.end
  }

  if (cursor < codePoints.length || segments.length === 0) {
    segments.push({ kind: 'text', text: codePoints.slice(cursor).join('') })
  }

  return segments
}
