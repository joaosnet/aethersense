import { useState } from 'react';
import { Moon, Sun, Menu, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { CSIPacket } from '../types/csi';
import { formatLocationAccuracy } from '../utils/dashboardMetrics';

export type DashboardTab = 'debug' | 'calibration' | 'client';

interface HeaderProps {
  data: CSIPacket | null;
  darkMode: boolean;
  onToggleDarkMode: () => void;
  activeTab: DashboardTab;
  onTabChange: (tab: DashboardTab) => void;
}

export function Header({ data, darkMode, onToggleDarkMode, activeTab, onTabChange }: HeaderProps) {
  const { t, i18n } = useTranslation();
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false);

  const presenceDetected = data?.detection?.presence?.detected;
  const location = data?.detection?.location;
  const occupancy = data?.detection?.occupancy;
  const accuracyLabel = formatLocationAccuracy(location);
  const nodesLabel = location?.nodes_used ? `${location.nodes_used}` : '--';

  const occupancyLabel = (() => {
    if (occupancy?.available) {
      if (occupancy.count === 0) return t('header.metrics.free');
      if (occupancy.count === 1) return `1 ${t('header.metrics.person')}`;
      const plus = occupancy.count >= (occupancy.probabilities?.length ?? 6) - 1 ? '+' : '';
      return `${occupancy.count}${plus} ${t('header.metrics.people')}`;
    }
    if (presenceDetected) return t('header.metrics.active');
    return data ? t('header.metrics.free') : '--';
  })();
  const occupancyActive = occupancy?.available ? occupancy.count > 0 : presenceDetected;
  const countUncalibrated = Boolean(occupancy?.available && occupancy.count_calibrated === false);

  const isEn = i18n.language.startsWith('en');

  const toggleLanguage = () => {
    const nextLang = isEn ? 'pt' : 'en';
    i18n.changeLanguage(nextLang);
    localStorage.setItem('i18nextLng', nextLang);
  };

  const handleTabSelect = (tab: DashboardTab) => {
    onTabChange(tab);
    setMobileMenuOpen(false);
  };

  return (
    <header className="header" style={{ padding: '8px 24px', position: 'sticky', top: 0, zIndex: 9999 }}>
      <div className="header__brand">
        <div className="header__logo" style={{ height: '72px' }}>
          <img
            className="header__logo-image"
            src="/logo/Logo Animado CSS.svg"
            alt="AetherSense"
            style={{ transform: 'scale(0.8)', transformOrigin: 'left center' }}
          />
        </div>
      </div>

      <div className="header__metrics" style={{ alignItems: 'center', gap: '16px' }}>
        {activeTab === 'debug' && (
        <div className="header__quick-metrics desktop-only">
          <div className="metric">
            <span className="metric__label">{t('header.metrics.packets')}</span>
            <span className="metric__value">{data?.count ?? '--'}</span>
          </div>

          <div className="metric">
            <span className="metric__label">{t('header.metrics.rssi')}</span>
            <span className="metric__value">{data?.rssi !== null && data?.rssi !== undefined ? `${data.rssi} dBm` : '--'}</span>
          </div>

          <div className="metric metric--presence">
            <span className="metric__label" style={{ display: 'flex', alignItems: 'center', gap: '4px' }}>
              {occupancy?.available ? t('header.metrics.occupancyMl') : t('header.metrics.presence')}
              {countUncalibrated && (
                <span
                  className="metric__est-badge"
                  title={t('header.metrics.uncalibratedTitle', 'Contagem do ML não calibrada para esta sala (modelo zero-shot)')}
                  aria-label={t('header.metrics.uncalibratedLabel', 'Estimativa não calibrada')}
                >
                  {t('header.metrics.estBadge', '~est')}
                </span>
              )}
            </span>
            <span
              className="metric__value"
              style={{ color: occupancyActive ? 'var(--accent-emerald)' : 'var(--text-muted)' }}
              title={occupancy?.available
                ? t('header.metrics.modelTooltip', 'Modelo {{model}} · confiança {{conf}}%{{suffix}}', { model: occupancy.model_run_id ?? '', conf: (100 * (occupancy.confidence ?? 0)).toFixed(0), suffix: countUncalibrated ? t('header.metrics.uncalibratedSuffix', ' · contagem não calibrada (zero-shot)') : '' })
                : undefined}
            >
              {occupancyLabel}
            </span>
          </div>

          <div className="metric">
            <span className="metric__label">{t('header.metrics.nodes')}</span>
            <span className="metric__value">{nodesLabel}</span>
          </div>

          <div className="metric">
            <span className="metric__label">{t('header.metrics.accuracy')}</span>
            <span className="metric__value">{accuracyLabel}</span>
          </div>
        </div>
        )}

        <div className="tabs-container desktop-only">
          <button
            className={`tab-btn ${activeTab === 'debug' ? 'active' : ''}`}
            onClick={() => handleTabSelect('debug')}
          >
            {t('header.tabs.debug')}
          </button>
          <button
            className={`tab-btn ${activeTab === 'calibration' ? 'active' : ''}`}
            onClick={() => handleTabSelect('calibration')}
          >
            {t('header.tabs.calibration')}
          </button>
          <button
            className={`tab-btn ${activeTab === 'client' ? 'active' : ''}`}
            onClick={() => handleTabSelect('client')}
          >
            {t('header.tabs.client')}
          </button>
        </div>

        <div className="header__actions" style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
          <button
            type="button"
            onClick={toggleLanguage}
            className="lang-toggle-btn"
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: '8px',
              background: 'transparent',
              border: 'none',
              cursor: 'pointer',
              padding: '4px 6px',
              borderRadius: '9999px',
              fontFamily: 'inherit',
            }}
            title={t('header.language', 'Trocar idioma / Change language')}
            aria-label={t('header.language', 'Trocar idioma / Change language')}
          >
            <span
              style={{
                fontSize: '14px',
                fontWeight: !isEn ? 800 : 600,
                color: !isEn ? 'var(--text-primary)' : 'var(--text-muted)',
                transition: 'color 0.2s ease, font-weight 0.2s ease',
                letterSpacing: '0.5px',
              }}
            >
              PT
            </span>

            <div
              style={{
                position: 'relative',
                width: '54px',
                height: '28px',
                borderRadius: '9999px',
                overflow: 'hidden',
                border: '2px solid var(--border)',
                boxShadow: 'inset 0 1px 3px rgba(0,0,0,0.25)',
                background: '#e0e0e0',
                display: 'flex',
                alignItems: 'center',
              }}
            >
              {!isEn ? (
                <svg viewBox="0 0 54 28" preserveAspectRatio="none" style={{ position: 'absolute', inset: 0, width: '100%', height: '100%' }}>
                  <rect width="54" height="28" fill="#009c3b" />
                  <polygon points="27,2 51,14 27,26 3,14" fill="#ffdf00" />
                  <circle cx="27" cy="14" r="7.5" fill="#002776" />
                  <path d="M 20 15 Q 27 12 34 13" stroke="#ffffff" strokeWidth="1" fill="none" />
                </svg>
              ) : (
                <svg viewBox="0 0 54 28" preserveAspectRatio="none" style={{ position: 'absolute', inset: 0, width: '100%', height: '100%' }}>
                  <rect width="54" height="28" fill="#012169" />
                  <line x1="0" y1="0" x2="54" y2="28" stroke="#FFFFFF" strokeWidth="6" />
                  <line x1="0" y1="28" x2="54" y2="0" stroke="#FFFFFF" strokeWidth="6" />
                  <line x1="0" y1="0" x2="54" y2="28" stroke="#C8102E" strokeWidth="2.5" />
                  <line x1="0" y1="28" x2="54" y2="0" stroke="#C8102E" strokeWidth="2.5" />
                  <line x1="27" y1="0" x2="27" y2="28" stroke="#FFFFFF" strokeWidth="8" />
                  <line x1="0" y1="14" x2="54" y2="14" stroke="#FFFFFF" strokeWidth="8" />
                  <line x1="27" y1="0" x2="27" y2="28" stroke="#C8102E" strokeWidth="4.5" />
                  <line x1="0" y1="14" x2="54" y2="14" stroke="#C8102E" strokeWidth="4.5" />
                </svg>
              )}

              <div
                style={{
                  position: 'absolute',
                  top: '2px',
                  left: !isEn ? '2px' : 'calc(100% - 24px)',
                  width: '22px',
                  height: '22px',
                  borderRadius: '50%',
                  background: '#ffffff',
                  boxShadow: '0 2px 5px rgba(0, 0, 0, 0.35), 0 0 1px rgba(0,0,0,0.2)',
                  transition: 'left 0.25s cubic-bezier(0.4, 0.0, 0.2, 1)',
                  zIndex: 2,
                }}
              />
            </div>

            <span
              style={{
                fontSize: '14px',
                fontWeight: isEn ? 800 : 600,
                color: isEn ? 'var(--text-primary)' : 'var(--text-muted)',
                transition: 'color 0.2s ease, font-weight 0.2s ease',
                letterSpacing: '0.5px',
              }}
            >
              EN
            </span>
          </button>

          <button
            className="btn btn--secondary"
            style={{ padding: '8px', display: 'flex', alignItems: 'center', borderRadius: '50%', border: 'none', background: 'transparent', cursor: 'pointer', minHeight: '38px', minWidth: '38px', justifyContent: 'center' }}
            onClick={onToggleDarkMode}
            title={t('header.theme', 'Alternar Tema')}
          >
            {darkMode ? <Sun size={20} color="var(--text-secondary)" /> : <Moon size={20} color="var(--text-secondary)" />}
          </button>

          <button
            className="hamburger-btn mobile-only"
            style={{ padding: '8px', display: 'flex', alignItems: 'center', justifyContent: 'center', borderRadius: '8px', border: '1px solid var(--border)', background: 'var(--bg-card)', cursor: 'pointer', color: 'var(--text-primary)', minHeight: '40px', minWidth: '40px' }}
            onClick={() => setMobileMenuOpen(!mobileMenuOpen)}
            aria-label={t('header.mobileMenu', 'Abrir menu mobile')}
          >
            {mobileMenuOpen ? <X size={22} /> : <Menu size={22} />}
          </button>
        </div>
      </div>

      {mobileMenuOpen && (
        <div className="mobile-nav-drawer">
          <div className="mobile-nav-drawer__section">
            <h4 className="mobile-nav-drawer__title">{t('header.navigation', 'Navegação')}</h4>
            <div className="mobile-nav-drawer__tabs">
              <button
                className={`tab-btn ${activeTab === 'debug' ? 'active' : ''}`}
                onClick={() => handleTabSelect('debug')}
                style={{ width: '100%', justifyContent: 'flex-start', padding: '12px 16px', fontSize: '15px' }}
              >
                {t('header.tabs.debug')}
              </button>
              <button
                className={`tab-btn ${activeTab === 'calibration' ? 'active' : ''}`}
                onClick={() => handleTabSelect('calibration')}
                style={{ width: '100%', justifyContent: 'flex-start', padding: '12px 16px', fontSize: '15px' }}
              >
                {t('header.tabs.calibration')}
              </button>
              <button
                className={`tab-btn ${activeTab === 'client' ? 'active' : ''}`}
                onClick={() => handleTabSelect('client')}
                style={{ width: '100%', justifyContent: 'flex-start', padding: '12px 16px', fontSize: '15px' }}
              >
                {t('header.tabs.client')}
              </button>
            </div>
          </div>

          {activeTab === 'debug' && (
            <div className="mobile-nav-drawer__section">
              <h4 className="mobile-nav-drawer__title">{t('header.realtimeMetrics', 'Métricas em Tempo Real')}</h4>
              <div className="mobile-nav-drawer__metrics">
                <div className="metric">
                  <span className="metric__label">{t('header.metrics.packets')}</span>
                  <span className="metric__value">{data?.count ?? '--'}</span>
                </div>
                <div className="metric">
                  <span className="metric__label">{t('header.metrics.rssi')}</span>
                  <span className="metric__value">{data?.rssi !== null && data?.rssi !== undefined ? `${data.rssi} dBm` : '--'}</span>
                </div>
                <div className="metric">
                  <span className="metric__label">{t('header.metrics.nodes')}</span>
                  <span className="metric__value">{nodesLabel}</span>
                </div>
                <div className="metric">
                  <span className="metric__label">{t('header.metrics.accuracy')}</span>
                  <span className="metric__value">{accuracyLabel}</span>
                </div>
                <div className="metric metric--presence" style={{ gridColumn: 'span 2' }}>
                  <span className="metric__label">
                    {occupancy?.available ? t('header.metrics.occupancyMl') : t('header.metrics.presence')}
                  </span>
                  <span
                    className="metric__value"
                    style={{ color: occupancyActive ? 'var(--accent-emerald)' : 'var(--text-muted)' }}
                  >
                    {occupancyLabel}
                  </span>
                </div>
              </div>
            </div>
          )}
        </div>
      )}
    </header>
  );
}
