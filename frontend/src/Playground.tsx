import { useMutation, useQuery } from '@tanstack/react-query'
import { useRef, useState } from 'react'

import { detectText, getHealth, type DetectResponse } from './api/detector'
import { DetectionDetails } from './components/DetectionDetails'
import { HealthStatus } from './components/HealthStatus'
import { HighlightedResult } from './components/HighlightedResult'
import { TextAnalyzer } from './components/TextAnalyzer'
import { buildHighlightedSegments, InvalidDetectionResponseError } from './domain/detections'

type AnalysisResult = {
  text: string
  response: DetectResponse
}

type ActiveRequest = {
  controller: AbortController
  generation: number
}

type PendingAnalysis = ActiveRequest & {
  text: string
}

function resultScanStatus(layersRun: DetectResponse['layers_run']) {
  const ranDeterministic = layersRun.includes('deterministic')
  const ranNer = layersRun.includes('ner')

  if (ranDeterministic && ranNer) {
    return { state: 'full', message: 'Full scan: deterministic + NER' }
  }
  if (ranDeterministic) {
    return { state: 'limited', message: 'Limited scan: deterministic only — NER did not run.' }
  }
  if (ranNer) {
    return { state: 'limited', message: 'Limited scan: NER only — deterministic detection did not run.' }
  }
  return { state: 'limited', message: 'Limited scan: no detector layers ran.' }
}

export function Playground() {
  const [text, setText] = useState('')
  const [result, setResult] = useState<AnalysisResult | null>(null)
  const [error, setError] = useState<Error | null>(null)
  const [inFlight, setInFlight] = useState(false)
  const generation = useRef(0)
  const activeRequest = useRef<ActiveRequest | null>(null)
  const pendingAnalyses = useRef(new Map<number, PendingAnalysis>())
  const healthQuery = useQuery({
    queryKey: ['detector-health'],
    queryFn: ({ signal }) => getHealth(signal),
    refetchInterval: 10_000,
  })
  const mutation = useMutation({
    gcTime: 0,
    mutationFn: (requestGeneration: number) => {
      const request = pendingAnalyses.current.get(requestGeneration)
      if (!request) {
        return Promise.reject(new DOMException('Analysis cancelled.', 'AbortError'))
      }
      return detectText({ text: request.text }, request.controller.signal)
    },
    onSuccess: (response, requestGeneration) => {
      const request = pendingAnalyses.current.get(requestGeneration)
      if (!request || requestGeneration !== generation.current) {
        return
      }
      try {
        buildHighlightedSegments(request.text, response.spans)
        setResult({ text: request.text, response })
      } catch (renderError) {
        if (renderError instanceof InvalidDetectionResponseError) {
          setError(renderError)
          return
        }
        throw renderError
      }
    },
    onError: (requestError, requestGeneration) => {
      if (!pendingAnalyses.current.has(requestGeneration) || requestGeneration !== generation.current) {
        return
      }
      setError(requestError)
    },
    onSettled: (_response, _requestError, requestGeneration) => {
      pendingAnalyses.current.delete(requestGeneration)
      if (activeRequest.current?.generation === requestGeneration) {
        activeRequest.current = null
        setInFlight(false)
      }
    },
  })

  function analyze() {
    if (inFlight || text.trim().length === 0) {
      return
    }

    const request: PendingAnalysis = {
      controller: new AbortController(),
      generation: generation.current + 1,
      text,
    }
    generation.current = request.generation
    activeRequest.current = { controller: request.controller, generation: request.generation }
    pendingAnalyses.current.set(request.generation, request)
    mutation.reset()
    setResult(null)
    setError(null)
    setInFlight(true)
    mutation.mutate(request.generation)
  }

  function clear() {
    generation.current += 1
    setText('')
    setResult(null)
    setError(null)
    mutation.reset()
    const request = activeRequest.current
    if (request) {
      pendingAnalyses.current.delete(request.generation)
      request.controller.abort()
    }
  }

  const scanStatus = result && resultScanStatus(result.response.layers_run)

  return (
    <main className="playground">
      <header className="playground__header">
        <div className="playground__brand">
          <p className="playground__wordmark">Tessera</p>
          <h1>Detector playground</h1>
        </div>
        <HealthStatus
          health={healthQuery.data}
          pending={healthQuery.isPending}
          unavailable={healthQuery.isError}
        />
      </header>
      <p className="playground__intro">
        Inspect local detector findings without saving the text you provide.
      </p>
      <TextAnalyzer
        text={text}
        pending={inFlight}
        onTextChange={setText}
        onAnalyze={analyze}
        onClear={clear}
      />
      <section className="analysis-output" aria-live="polite" aria-atomic="true">
        {inFlight && <p>Analyzing text…</p>}
        {!inFlight && !result && !error && <p>Detections will appear here after you analyze text.</p>}
        {result && (
          <>
            <p className="result-scan-status" data-state={scanStatus?.state}>{scanStatus?.message}</p>
            <HighlightedResult text={result.text} spans={result.response.spans} />
            {result.response.spans.length > 0 && (
              <DetectionDetails text={result.text} spans={result.response.spans} />
            )}
          </>
        )}
      </section>
      {error && <p role="alert">{error.message}</p>}
    </main>
  )
}
