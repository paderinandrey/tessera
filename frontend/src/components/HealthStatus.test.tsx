import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import type { HealthResponse } from '../api/detector'
import { HealthStatus } from './HealthStatus'

describe('HealthStatus', () => {
  afterEach(cleanup)

  it('shows a checking state while detector health is pending', () => {
    render(<HealthStatus pending unavailable={false} health={undefined} />)

    const status = screen.getByRole('status')
    expect(status).toHaveAttribute('data-state', 'checking')
    expect(screen.getByText(/Checking detector health/i)).toBeVisible()
  })

  it('does not describe an unavailable detector as degraded NER', () => {
    render(<HealthStatus pending={false} unavailable health={undefined} />)

    const status = screen.getByRole('status')
    expect(status).toHaveAttribute('data-state', 'unavailable')
    expect(screen.getByText(/Detector unavailable/i)).toBeVisible()
    expect(screen.queryByText(/Deterministic detection active/i)).not.toBeInTheDocument()
  })

  it('explains that deterministic detection remains when NER is off', () => {
    const health: HealthResponse = {
      status: 'ok',
      ner: false,
      ner_off_reason: 'weights unavailable',
    }
    render(<HealthStatus pending={false} unavailable={false} health={health} />)

    const status = screen.getByRole('status')
    expect(status).toHaveAttribute('data-state', 'deterministic')
    expect(screen.getByText(/Deterministic detection active/i)).toBeVisible()
    expect(screen.getByText('weights unavailable')).toBeVisible()
  })

  it('uses the fallback reason when NER is off without a supplied reason', () => {
    const health: HealthResponse = {
      status: 'ok',
      ner: false,
      ner_off_reason: null,
    }
    render(<HealthStatus pending={false} unavailable={false} health={health} />)

    expect(screen.getByText('NER is unavailable.')).toBeVisible()
  })

  it('reports full detection when the NER layer is available', () => {
    const health: HealthResponse = {
      status: 'ok',
      ner: true,
      ner_off_reason: null,
    }
    render(<HealthStatus pending={false} unavailable={false} health={health} />)

    const status = screen.getByRole('status')
    expect(status).toHaveAttribute('data-state', 'full')
    expect(screen.getByText(/Full detection active/i)).toBeVisible()
  })
})
