import type { DetectorSpan } from '../api/detector'
import { classifyRecognizer, type DetectionLayer } from '../domain/detections'

type DetectionDetailsProps = {
  text: string
  spans: DetectorSpan[]
}

const confidenceFormatter = new Intl.NumberFormat(undefined, {
  style: 'percent',
  maximumFractionDigits: 0,
})

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

function fragmentFor(text: string, span: DetectorSpan): string {
  return Array.from(text).slice(span.start, span.end).join('')
}

export function DetectionDetails({ text, spans }: DetectionDetailsProps) {
  return (
    <table className="detection-details">
      <caption>Detection details</caption>
      <thead>
        <tr>
          <th scope="col">Fragment</th>
          <th scope="col">Entity type</th>
          <th scope="col">Layer</th>
          <th scope="col">Confidence</th>
          <th scope="col">Tier</th>
          <th scope="col">Recognizer</th>
          <th scope="col">Context</th>
        </tr>
      </thead>
      <tbody>
        {spans.map((span, index) => (
          <tr key={`${span.start}-${span.end}-${span.recognizer}-${index}`}>
            <td data-label="Fragment">{fragmentFor(text, span)}</td>
            <td data-label="Entity type">{span.entity_type}</td>
            <td data-label="Layer">{layerLabel(classifyRecognizer(span.recognizer))}</td>
            <td data-label="Confidence">
              <span aria-label={`Confidence ${span.confidence}`}>
                {confidenceFormatter.format(span.confidence)}
              </span>
            </td>
            <td data-label="Tier">{span.tier}</td>
            <td data-label="Recognizer">{span.recognizer}</td>
            <td data-label="Context">Context boosted: {span.boosted ? 'yes' : 'no'}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
