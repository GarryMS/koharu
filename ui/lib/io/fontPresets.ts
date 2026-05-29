import { fetchApi } from '@/lib/api/fetch'
import type { TextStyle } from '@/lib/api/schemas'

export interface FontPreset {
  id: string
  name: string
  style: TextStyle
}

export interface CreateFontPresetRequest {
  name: string
  style: TextStyle
}

export const listFontPresets = () => fetchApi<FontPreset[]>('/api/v1/font-presets')

export const createFontPreset = (body: CreateFontPresetRequest) =>
  fetchApi<FontPreset>('/api/v1/font-presets', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })

export const deleteFontPreset = (id: string) =>
  fetchApi<void>(`/api/v1/font-presets/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  })
