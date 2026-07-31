import type { QueryClient } from '@tanstack/react-query'
import { createRootRouteWithContext, Link, Outlet } from '@tanstack/react-router'
import { Activity, Images, MessagesSquare, TrendingUp } from 'lucide-react'
import styles from './root.module.css'

export interface RouterContext {
  queryClient: QueryClient
}

export const Route = createRootRouteWithContext<RouterContext>()({
  component: RootLayout,
  notFoundComponent: () => <main className={styles.notFound}>That page does not exist.</main>,
})

function RootLayout() {
  return (
    <div className={styles.app}>
      <header className={styles.header}>
        <Link to="/chat" className={styles.brand}>QQ Archive</Link>
        <nav aria-label="Primary navigation" className={styles.nav}>
          <Link to="/chat" activeProps={{ className: styles.active }}>
            <MessagesSquare size={18} /> Chat
          </Link>
          <Link to="/images" activeProps={{ className: styles.active }}>
            <Images size={18} /> Images
          </Link>
          <Link to="/images/trends" activeProps={{ className: styles.active }}>
            <TrendingUp size={18} /> 热门趋势
          </Link>
          <Link to="/tasks" activeProps={{ className: styles.active }}>
            <Activity size={18} /> Tasks
          </Link>
        </nav>
      </header>
      <Outlet />
    </div>
  )
}
