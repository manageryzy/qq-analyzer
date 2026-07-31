import { Link } from '@tanstack/react-router'
import { Download, ExternalLink, File, Image as ImageIcon, ScanSearch } from 'lucide-react'
import { useState, type ReactNode } from 'react'
import styles from './RichMessage.module.css'

export type RichNode = {
  type?: string
  text?: string
  label?: string
  raw_text?: string
  href?: string
  title?: string
  summary?: string
  source?: string
  cover?: string
  image_index_id?: number
  similar_href?: string
  asset?: MediaAsset | null
  assets?: MediaAsset[]
  children?: RichChild[]
  items?: RichChild[]
  items_expanded?: RichChild[]
  nodes?: RichChild[]
  rich_nodes?: RichChild[]
  display_sender?: string
  display_sender_line?: string
  datetime?: string
  msg_seq?: number | string
  item_count?: number
  preview_count?: number
  file_name?: string
  res_id?: string
  expand_reason?: string
  file_meta?: Record<string, unknown>
  asset_reason?: string
  [key: string]: unknown
}

export type RichChild = RichNode | string

export type MediaAsset = {
  kind?: string
  name?: string
  href?: string
  path?: string
  image_index_id?: number
  similar_href?: string
}

type AssetCursor = { assets: MediaAsset[]; used: Set<MediaAsset> }

const faceGlyphs: Record<string, string> = {
  '[爱心]': '❤', '[心碎]': '💔', '[玫瑰]': '🌹', '[凋谢]': '🥀', '[太阳]': '☀️', '[月亮]': '🌙',
  '[礼物]': '🎁', '[蛋糕]': '🎂', '[足球]': '⚽', '[篮球]': '🏀', '[啤酒]': '🍺', '[咖啡]': '☕',
  '[饭]': '🍚', '[便便]': '💩', '[刀]': '🔪', '[炸弹]': '💣', '[闪电]': '⚡', '[OK]': '👌',
  '[NO]': '🙅', '[强]': '👍', '[弱]': '👎', '[握手]': '🤝', '[胜利]': '✌️',
}

function SimilarAction({ id }: { id?: number }) {
  if (!id) return null
  return (
    <Link to="/images" search={{ similar: id }} aria-label={`查找与图片 ${id} 相似的图片`} className={styles.similar}>
      <ScanSearch size={14} /> 图库 / 相似图片
    </Link>
  )
}

function FileActions({ asset, label }: { asset: MediaAsset; label: string }) {
  if (!asset.href) return null
  return (
    <span className={styles.actions}>
      <a href={asset.href} target="_blank" rel="noreferrer"><ExternalLink size={13} /> 打开</a>
      <a href={asset.href} download={asset.name || label}><Download size={13} /> 下载</a>
      <SimilarAction id={asset.image_index_id} />
    </span>
  )
}

function ImageAsset({ asset, face = false }: { asset: MediaAsset; face?: boolean }) {
  const [failed, setFailed] = useState(false)
  if (!asset.href || failed) return <span className={styles.missing}><ImageIcon size={15} /> 图片不可用</span>
  const image = <img src={asset.href} alt={asset.name || (face ? '表情' : '聊天图片')} loading="lazy" onError={() => setFailed(true)} />
  return (
    <figure className={`${styles.media} ${face ? styles.face : ''}`}>
      {!face && asset.image_index_id
        ? <Link to="/images" search={{ similar: asset.image_index_id }} className={styles.indexedImage}>{image}<span><ScanSearch size={14} /> 在图库中查看相似图片</span></Link>
        : <a href={asset.href} target="_blank" rel="noreferrer">{image}</a>}
      <FileActions asset={asset} label={face ? '表情' : '图片'} />
    </figure>
  )
}

function AssetView({ asset, expectedKind }: { asset: MediaAsset; expectedKind?: string }) {
  const href = asset.href
  const kind = asset.kind || expectedKind || 'file'
  if (kind === 'image' || kind === 'face') return <ImageAsset asset={asset} face={kind === 'face'} />
  if (!href) return <span className={styles.missing}>媒体文件不可用</span>
  if (kind === 'video') {
    return <div className={styles.mediaBox}><video className={styles.video} src={href} controls preload="metadata" /><FileActions asset={asset} label="视频" /></div>
  }
  if (kind === 'voice') {
    return <div className={styles.mediaBox}><audio src={href} controls preload="none" /><FileActions asset={asset} label="音频" /></div>
  }
  return (
    <span className={styles.fileCard}>
      <File size={18} />
      <span>{asset.name || '文件'}</span>
      <FileActions asset={asset} label="文件" />
    </span>
  )
}

function childGroups(node: RichNode): RichChild[][] {
  return ['children', 'items_expanded', 'items', 'nodes', 'rich_nodes']
    .map((key) => node[key])
    .filter((value): value is RichChild[] => Array.isArray(value))
}

function hasInlineMedia(nodes: RichChild[]): boolean {
  return nodes.some((node) => {
    if (typeof node === 'string') return false
    if (['image', 'face', 'video', 'voice', 'file'].includes(node.type || '')) return true
    return childGroups(node).some(hasInlineMedia)
  })
}

function takeAsset(node: RichNode, cursor: AssetCursor): MediaAsset | undefined {
  const direct = [node.asset, ...(node.assets ?? [])].find((asset): asset is MediaAsset => Boolean(asset?.href))
  if (direct) {
    cursor.used.add(direct)
    const same = cursor.assets.find((asset) => asset.href === direct.href)
    if (same) cursor.used.add(same)
    return direct
  }
  const expected = node.type
  const candidate = cursor.assets.find((asset) => {
    if (cursor.used.has(asset)) return false
    const kind = asset.kind || ''
    return kind === expected || (expected === 'image' && !kind) || (expected === 'face' && kind === 'image')
  })
  if (candidate) cursor.used.add(candidate)
  return candidate
}

function previewText(items: RichChild[]) {
  return items.slice(0, 3).map((item) => typeof item === 'string' ? item : item.text).filter(Boolean).join('\n')
}

function RichNodeView({ node, cursor }: { node: RichChild; cursor: AssetCursor }): ReactNode {
  if (typeof node === 'string') return <span className={styles.multiItem}>{node}</span>
  const children = childGroups(node).flat()
  const text = node.text || ''
  if (node.type === 'text') return text
  if (node.type === 'newline') return <br />
  if (node.type === 'mention') return <span className={styles.mention}>{text}</span>
  if (node.type === 'quote') {
    return node.href
      ? <a className={styles.quote} href={node.href} title="跳转到引用消息">{text}</a>
      : <span className={styles.quote}>{text}</span>
  }
  if (node.type === 'rich' && node.href) {
    return (
      <a className={styles.richCard} href={node.href} target="_blank" rel="noreferrer">
        {node.cover && <img src={node.cover} alt="" loading="lazy" />}
        <span><strong>{node.title || text || node.href}</strong>{node.summary && <small>{node.summary}</small>}{node.source && <em>{node.source}</em>}</span>
      </a>
    )
  }
  if (node.type === 'image' || node.type === 'face' || node.type === 'video' || node.type === 'voice' || node.type === 'file') {
    const asset = takeAsset(node, cursor)
    if (asset) return <AssetView asset={asset} expectedKind={node.type} />
    if (node.type === 'face') return <span className={styles.faceGlyph} title={text}>{faceGlyphs[text] || text}</span>
    const detail = node.asset_reason || node.label || `${node.type} 未匹配到本地文件`
    return <span className={styles.missing}><ImageIcon size={15} /> {detail}</span>
  }
  if (node.type === 'multi_msg' || node.type === 'record') {
    const items = node.items_expanded?.length ? node.items_expanded : node.items ?? children
    const displayedCount = items.length || node.preview_count || 0
    const totalCount = node.item_count || items.length || 0
    return (
      <details className={styles.record}>
        <summary>
          <span>{node.label || text || '[聊天记录]'}</span>
          <small>{displayedCount}/{totalCount} 条</small>
          {previewText(node.items ?? []) && <em>{previewText(node.items ?? [])}</em>}
        </summary>
        <div className={styles.recordBody}>{items.length
          ? items.map((child, index) => <RichNodeView key={index} node={child} cursor={cursor} />)
          : <span className={styles.recordEmpty}>没有本地预览项</span>}</div>
        {(node.file_name || node.res_id || node.expand_reason) && <footer>{[node.file_name, node.res_id, node.expand_reason].filter(Boolean).join(' · ')}</footer>}
      </details>
    )
  }
  if (node.type === 'nested') {
    return <section className={styles.nested}><strong>{text || '聊天记录'}</strong><div className={styles.nestedBody}>{children.length ? children.map((child, index) => <RichNodeView key={index} node={child} cursor={cursor} />) : node.raw_text}</div></section>
  }
  if (node.type === 'mmp_item') {
    const sender = node.display_sender_line || node.display_sender || '内嵌消息'
    return <section className={`${styles.nested} ${styles.mmpItem}`}>
      <header className={styles.mmpHeader}>
        <strong>{sender}</strong>
        {node.datetime && <time>{node.datetime}</time>}
        {node.msg_seq !== undefined && <small>seq {String(node.msg_seq)}</small>}
      </header>
      <div className={styles.nestedBody}>{children.length
        ? children.map((child, index) => <RichNodeView key={index} node={child} cursor={cursor} />)
        : text}</div>
    </section>
  }
  if (node.type === 'multi_item') return <span className={styles.multiItem}>{text}</span>
  if (children.length) return children.map((child, index) => <RichNodeView key={index} node={child} cursor={cursor} />)
  return text ? <span className={styles.unknownNode}>{text}</span> : null
}

export function RichMessage({
  nodes,
  assets = [],
  fallback,
  mediaKind,
  mediaLabel,
  unmatchedReason,
}: {
  nodes?: RichChild[]
  assets?: MediaAsset[]
  fallback: string
  mediaKind?: string
  mediaLabel?: string
  unmatchedReason?: string
}) {
  const cursor: AssetCursor = { assets: [...assets], used: new Set() }
  const body = nodes?.length
    ? <div className={styles.rich}>{nodes.map((node, index) => <RichNodeView key={index} node={node} cursor={cursor} />)}</div>
    : <p className={styles.text}>{fallback}</p>
  // Rich-node assets are consumed while React renders RichNodeView. Avoid also
  // emitting the message-level fallback list when the payload already contains
  // media nodes; messages without inline media still retain the legacy fallback.
  const remaining = nodes?.length && hasInlineMedia(nodes) ? [] : cursor.assets
  return (
    <>
      {body}
      {remaining.length > 0 && <div className={styles.assets}>{remaining.map((asset, index) => <AssetView key={`${asset.href}-${index}`} asset={asset} />)}</div>}
      {!remaining.length && mediaKind && !assets.length && <div className={styles.placeholder}>[{mediaLabel || mediaKind}：未匹配到本地文件] {unmatchedReason}</div>}
    </>
  )
}
