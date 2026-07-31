import { Link } from '@tanstack/react-router'
import { Copy, Images, Info, MessageSquare, ScanSearch } from 'lucide-react'
import { useState } from 'react'
import styles from './ImageCard.module.css'

export interface GalleryAsset {
  id: number
  thumbnail_url: string
  width?: number | null
  height?: number | null
  format?: string
  source?: string
  source_class?: string
  quality_flags?: string
  copy_count?: number
  variant_count?: number
  reference_count?: number
  score?: number
  match_kind?: string
  distance?: number | null
  error?: string | null
}

export function ImageCard({ asset, selected = false, onSelect }: { asset: GalleryAsset; selected?: boolean; onSelect: (asset: GalleryAsset) => void }) {
  const [previewFailed, setPreviewFailed] = useState(false)
  return (
    <article className={`${styles.card} ${selected ? styles.selected : ''}`}>
      <button type="button" className={styles.preview} onClick={() => onSelect(asset)} aria-label={`查看图片 ${asset.id} 的相似图片与聊天位置`} aria-pressed={selected}>
        {!previewFailed && <img src={asset.thumbnail_url} alt={`索引图片 ${asset.id}`} loading="lazy" onError={() => setPreviewFailed(true)} />}
        {(asset.error || previewFailed) && <span className={styles.corrupt}>缩略图不可用</span>}
        <span className={styles.openHint}><ScanSearch size={14} /> 相似图片与聊天位置</span>
      </button>
      <div className={styles.meta}>
        <span>{formatLabel(asset.format, asset.match_kind)} · {asset.width ?? '?'}×{asset.height ?? '?'}</span>
        {asset.score !== undefined && <strong>{Math.round(Math.min(1, Math.max(0, asset.score)) * 100)}%</strong>}
      </div>
      <div className={styles.badges}>
        {(asset.variant_count ?? 0) > (asset.copy_count ?? 1) && <span title="已合并相同画面的缩略图与原图"><Images size={12} /> {asset.variant_count} 个尺寸</span>}
        {(asset.copy_count ?? 0) > 1 && <span><Copy size={12} /> {asset.copy_count}</span>}
        {(asset.reference_count ?? 0) > 0 && <span><MessageSquare size={12} /> {asset.reference_count}</span>}
        {asset.match_kind && <span>{matchLabel(asset.match_kind)}</span>}
        {!asset.match_kind && asset.quality_flags && <span>{asset.quality_flags}</span>}
      </div>
      <div className={styles.actions}>
        <button type="button" onClick={() => onSelect(asset)}><ScanSearch size={14} /> 相似与聊天</button>
        <Link to="/images/$assetId" params={{ assetId: String(asset.id) }} aria-label={`图片 ${asset.id} 技术详情`}><Info size={14} /></Link>
      </div>
    </article>
  )
}

function formatLabel(format?: string, matchKind?: string) {
  return format || (matchKind ? matchLabel(matchKind) : '图像')
}

function matchLabel(kind: string) {
  const labels: Record<string, string> = {
    exact: '完全相同',
    same_sscd: '同一图片',
    copy_sscd: '视觉副本',
    near_hash: '像素近似',
    tile_hash: '局部相似',
    semantic_clip: '语义相似',
  }
  return labels[kind] || kind
}
