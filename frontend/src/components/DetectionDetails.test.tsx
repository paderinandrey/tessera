import { cleanup, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import type { DetectorSpan } from '../api/detector'
import { DetectionDetails } from './DetectionDetails'

describe('DetectionDetails', () => {
  afterEach(cleanup)

  it('shows complete metadata for a model detection', () => {
    const spans: DetectorSpan[] = [{
      start: 0,
      end: 7,
      entity_type: 'PERSON',
      recognizer: 'ner:gliner',
      confidence: 0.91,
      tier: 2,
      boosted: false,
    }]

    render(<DetectionDetails text="Martina" spans={spans} />)

    const row = screen.getAllByRole('row')[1]
    expect(within(row).getByText('Martina')).toBeVisible()
    expect(within(row).getByText('PERSON')).toBeVisible()
    expect(within(row).getByText('Model detected')).toBeVisible()
    expect(within(row).getByText('91%')).toHaveAccessibleName('Confidence 0.91')
    expect(within(row).getByText('2')).toBeVisible()
    expect(within(row).getByText('ner:gliner')).toBeVisible()
    expect(within(row).getByText('Context boosted: no')).toBeVisible()
  })

  it('identifies a context-boosted catalog detection', () => {
    const spans: DetectorSpan[] = [{
      start: 5,
      end: 9,
      entity_type: 'CREDIT_CARD',
      recognizer: 'catalog:credit_card',
      confidence: 1,
      tier: 1,
      boosted: true,
    }]

    render(<DetectionDetails text="Card 4111" spans={spans} />)

    const row = screen.getAllByRole('row')[1]
    expect(within(row).getByText('4111')).toBeVisible()
    expect(within(row).getByText('Checksum verified')).toBeVisible()
    expect(within(row).getByText('Context boosted: yes')).toBeVisible()
  })
})
