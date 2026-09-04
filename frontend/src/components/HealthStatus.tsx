import type { HealthResponse } from '../api/detector'

type HealthStatusProps = {
  health?: HealthResponse
  pending: boolean
  unavailable: boolean
}

export function HealthStatus({ health, pending, unavailable }: HealthStatusProps) {
  let state: 'checking' | 'unavailable' | 'deterministic' | 'full'
  let message: string
  let reason: string | undefined

  if (pending) {
    state = 'checking'
    message = 'Checking detector health…'
  } else if (unavailable || !health) {
    state = 'unavailable'
    message = 'Detector unavailable.'
  } else if (!health.ner) {
    state = 'deterministic'
    message = 'Deterministic detection active.'
    reason = health.ner_off_reason ?? 'NER is unavailable.'
  } else {
    state = 'full'
    message = 'Full detection active.'
  }

  return (
    <div className="health-status" role="status" aria-live="polite" data-state={state}>
      <span>{message}</span>
      {reason && <span> {reason}</span>}
    </div>
  )
}
