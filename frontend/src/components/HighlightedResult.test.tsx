import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import type { DetectorSpan } from '../api/detector'
import { HighlightedResult } from './HighlightedResult'

describe('HighlightedResult', () => {
  afterEach(cleanup)

  it('renders catalog and NER detections as labelled highlights', () => {
    const spans: DetectorSpan[] = [
      {
        start: 8,
        end: 27,
        entity_type: 'CREDIT_CARD',
        recognizer: 'catalog:credit_card',
        confidence: 1,
        tier: 1,
        boosted: false,
      },
      {
        start: 36,
        end: 43,
        entity_type: 'PERSON',
        recognizer: 'ner:gliner',
        confidence: 0.91,
        tier: 2,
        boosted: false,
      },
    ]

    render(<HighlightedResult text="Card is 4111 1111 1111 1111; holder Martina." spans={spans} />)

    const marks = screen.getAllByRole('mark')
    expect(marks).toHaveLength(2)
    expect(marks.map((mark) => mark.textContent)).toEqual(['4111 1111 1111 1111', 'Martina'])
    expect(screen.getByLabelText('4111 1111 1111 1111 — Checksum verified')).toBe(marks[0])
    expect(screen.getByLabelText('Martina — Model detected')).toBe(marks[1])
  })

  it('shows a safe error instead of partial highlights for overlapping offsets', () => {
    const spans: DetectorSpan[] = [
      {
        start: 0,
        end: 4,
        entity_type: 'PERSON',
        recognizer: 'ner:gliner',
        confidence: 0.91,
        tier: 2,
        boosted: false,
      },
      {
        start: 2,
        end: 6,
        entity_type: 'PERSON',
        recognizer: 'ner:gliner',
        confidence: 0.91,
        tier: 2,
        boosted: false,
      },
    ]

    render(<HighlightedResult text="Martina" spans={spans} />)

    expect(screen.getByRole('alert')).toHaveTextContent('Detector returned invalid span offsets.')
    expect(screen.queryByRole('mark')).not.toBeInTheDocument()
  })

  it('explains the limitation when no spans are returned', () => {
    render(<HighlightedResult text="No names here." spans={[]} />)

    expect(screen.getByText('No entities were detected. This does not prove the text contains no personal data.'))
      .toBeVisible()
  })
})
