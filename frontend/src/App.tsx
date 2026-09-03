import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useState } from 'react'

import { Playground } from './Playground'

type AppProps = {
  queryClient?: QueryClient
}

export function App({ queryClient: providedQueryClient }: AppProps) {
  const [queryClient] = useState(() => providedQueryClient ?? new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false, gcTime: 0 },
    },
  }))

  return (
    <QueryClientProvider client={queryClient}>
      <Playground />
    </QueryClientProvider>
  )
}
