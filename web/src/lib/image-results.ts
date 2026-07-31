export type DuplicateMode = 'variants' | 'collapsed' | 'all' | 'duplicates' | 'unique'

export type GroupedSearchResult = {
  id: number
  exact_representative_id?: number
  copy_count?: number
  sha256?: string | null
  phash?: string | null
  quality_flags?: string
  score?: number
  match_kind?: string
}

function exactKey(item: GroupedSearchResult) {
  // The content digest is the final authority when a search report was built
  // from an older/stale exact-group snapshot. This also makes text, upload,
  // SSCD and CLIP result lists agree even if they returned different asset IDs.
  if (item.sha256) return `sha256:${item.sha256.toLowerCase()}`
  return `exact:${item.exact_representative_id ?? item.id}`
}

/** Apply the same duplicate semantics to search results as the paged gallery.
 * Exact groups are always authoritative. Variants additionally groups exact
 * pHash matches, covering thumbnails and byte-different re-encodes of one
 * visual. Users can select `collapsed` when only SHA256 identity is desired.
 */
export function applySearchDuplicateMode<T extends GroupedSearchResult>(items: T[], mode: DuplicateMode): T[] {
  if (mode === 'all') return items

  const eligible = mode === 'duplicates'
    ? items.filter((item) => (item.copy_count ?? 1) > 1)
    : mode === 'unique'
      ? items.filter((item) => (item.copy_count ?? 1) === 1)
      : items

  if (mode === 'unique') return eligible

  const seen = new Set<string>()
  return eligible.filter((item) => {
    const key = mode === 'variants' && (item.match_kind === 'same_sscd' || (item.match_kind === 'copy_sscd' && (item.score ?? 0) >= 0.98))
      ? 'sscd:query-image'
      : mode === 'variants' && item.phash
        ? `phash:${item.phash.toLowerCase()}`
        : exactKey(item)
    if (seen.has(key)) return false
    seen.add(key)
    return true
  })
}
