type TextAnalyzerProps = {
  text: string
  pending: boolean
  onTextChange(text: string): void
  onAnalyze(): void
  onClear(): void
}

export function TextAnalyzer({
  text,
  pending,
  onTextChange,
  onAnalyze,
  onClear,
}: TextAnalyzerProps) {
  return (
    <section className="analyzer-panel">
      <label htmlFor="detector-text">Text to inspect</label>
      <textarea
        id="detector-text"
        value={text}
        onChange={(event) => onTextChange(event.target.value)}
      />
      <div className="analyzer-panel__actions">
        <button type="button" disabled={pending || text.trim().length === 0} onClick={onAnalyze}>
          Analyze text
        </button>
        <button type="button" onClick={onClear}>Clear</button>
      </div>
    </section>
  )
}
