import type { DetectorSpan } from '../api/detector'
import {
  buildHighlightedSegments,
  type DetectionLayer,
  InvalidDetectionResponseError,
} from '../domain/detections'

type HighlightedResultProps = {
  text: string
  spans: DetectorSpan[]
}

function layerLabel(layer: DetectionLayer): string {
  switch (layer) {
    case 'deterministic':
      return 'Checksum verified'
    case 'ner':
      return 'Model detected'
    case 'other':
      return 'Other recognizer'
  }
}

export function HighlightedResult({ text, spans }: HighlightedResultProps) {
  let segments

  try {
    segments = buildHighlightedSegments(text, spans)
  } catch (error) {
    if (error instanceof InvalidDetectionResponseError) {
      return <p role="alert">{error.message}</p>
    }
    throw error
  }

  if (spans.length === 0) {
    return (
      <p className="highlighted-result">
        No entities were detected. This does not prove the text contains no personal data.
      </p>
    )
  }

  return (
    <p className="highlighted-result">
      {segments.map((segment, index) => {
        if (segment.kind === 'text') {
          return <span key={index}>{segment.text}</span>
        }

        const label = layerLabel(segment.layer)
        return (
          <mark key={index} data-layer={segment.layer} aria-label={`${segment.text} — ${label}`}>
            {segment.text}
          </mark>
        )
      })}
    </p>
  )
}
