import { Settings2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { CSISettings } from '../types/csi';

interface ControlsDrawerProps {
  isOpen?: boolean;
  onToggle?: () => void;
  settings: CSISettings;
  onSettingsChange: (settings: CSISettings) => void;
  feedbackMessage?: string;
  feedbackTone?: 'neutral' | 'success' | 'error';
  // Quando `inline`, renderiza como card normal (sem drawer flutuante nem
  // botão-toggle) para entrar na grade da aba "Ajustes & Calibração".
  inline?: boolean;
}

export function ControlsDrawer({ isOpen, onToggle, settings, onSettingsChange, feedbackMessage, feedbackTone, inline = false }: ControlsDrawerProps) {
  const { t } = useTranslation();
  const handleChange = (key: keyof CSISettings, value: number) => {
    onSettingsChange({ ...settings, [key]: value });
  };

  const body = (
      <div className="controls-body">
        <h3 className="controls__title">{t('controls.processingFilters', 'Filtros de Processamento')}</h3>

        <label className="control">
          <span className="control__label">{t('controls.heatmap', 'Heatmap')}</span>
          <input
            type="range"
            min="20"
            max="180"
            step="1"
            value={settings.heatmap_window}
            onChange={(e) => handleChange('heatmap_window', parseInt(e.target.value, 10))}
          />
          <output>{settings.heatmap_window}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.stft', 'STFT')}</span>
          <input
            type="range"
            min="16"
            max="180"
            step="1"
            value={settings.stft_window}
            onChange={(e) => handleChange('stft_window', parseInt(e.target.value, 10))}
          />
          <output>{settings.stft_window}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.pca', 'PCA')}</span>
          <input
            type="range"
            min="20"
            max="180"
            step="1"
            value={settings.pca_window}
            onChange={(e) => handleChange('pca_window', parseInt(e.target.value, 10))}
          />
          <output>{settings.pca_window}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.padding', 'Padding')}</span>
          <input
            type="range"
            min="0"
            max="60"
            step="1"
            value={settings.padding * 100}
            onChange={(e) => handleChange('padding', parseInt(e.target.value, 10) / 100)}
          />
          <output>{(settings.padding).toFixed(2)}</output>
        </label>

        <h3 className="controls__title" style={{ marginTop: '1.25rem' }}>{t('controls.fftVitals', 'Heurística FFT & Vitais')}</h3>

        <label className="control">
          <span className="control__label">{t('controls.vitalsWindow', 'Janela Vitais (s)')}</span>
          <input
            type="range"
            min="10"
            max="120"
            step="5"
            value={settings.vitals_window_seconds ?? 30}
            onChange={(e) => handleChange('vitals_window_seconds', parseFloat(e.target.value))}
          />
          <output>{settings.vitals_window_seconds ?? 30}s</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.minFftSamples', 'Min Amostras FFT')}</span>
          <input
            type="range"
            min="30"
            max="300"
            step="10"
            value={settings.vitals_min_samples ?? 100}
            onChange={(e) => handleChange('vitals_min_samples', parseInt(e.target.value, 10))}
          />
          <output>{settings.vitals_min_samples ?? 100}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.topKSubcarriers', 'Top-K Subportadoras')}</span>
          <input
            type="range"
            min="1"
            max="64"
            step="1"
            value={settings.vitals_subcarrier_top_k ?? 16}
            onChange={(e) => handleChange('vitals_subcarrier_top_k', parseInt(e.target.value, 10))}
          />
          <output>{settings.vitals_subcarrier_top_k ?? 16}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.peakProminence', 'Proeminência de Pico')}</span>
          <input
            type="range"
            min="1.0"
            max="8.0"
            step="0.2"
            value={settings.vitals_peak_prominence ?? 2.0}
            onChange={(e) => handleChange('vitals_peak_prominence', parseFloat(e.target.value))}
          />
          <output>{(settings.vitals_peak_prominence ?? 2.0).toFixed(1)}x</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.heartSnrFloor', 'Piso SNR Cardíaco')}</span>
          <input
            type="range"
            min="1.0"
            max="10.0"
            step="0.5"
            value={settings.heart_rate_abs_snr_floor ?? 3.0}
            onChange={(e) => handleChange('heart_rate_abs_snr_floor', parseFloat(e.target.value))}
          />
          <output>{(settings.heart_rate_abs_snr_floor ?? 3.0).toFixed(1)}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.rejectedHarmonics', 'Harmônicos Rejeitados')}</span>
          <input
            type="range"
            min="0"
            max="10"
            step="1"
            value={settings.vitals_harmonic_count ?? 5}
            onChange={(e) => handleChange('vitals_harmonic_count', parseInt(e.target.value, 10))}
          />
          <output>{settings.vitals_harmonic_count ?? 5}</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.harmonicTolerance', 'Tolerância Harm. (Hz)')}</span>
          <input
            type="range"
            min="0.01"
            max="0.15"
            step="0.01"
            value={settings.vitals_harmonic_tolerance_hz ?? 0.06}
            onChange={(e) => handleChange('vitals_harmonic_tolerance_hz', parseFloat(e.target.value))}
          />
          <output>{(settings.vitals_harmonic_tolerance_hz ?? 0.06).toFixed(2)} Hz</output>
        </label>

        <h3 className="controls__title" style={{ marginTop: '1.25rem' }}>{t('controls.frequencyRanges', 'Faixas de Frequência (Hz)')}</h3>

        <label className="control">
          <span className="control__label">{t('controls.breathingMin', 'Respiração Mín')}</span>
          <input
            type="range"
            min="0.05"
            max="0.30"
            step="0.01"
            value={settings.breathing_freq_min_hz ?? 0.10}
            onChange={(e) => handleChange('breathing_freq_min_hz', parseFloat(e.target.value))}
          />
          <output>{(settings.breathing_freq_min_hz ?? 0.10).toFixed(2)} Hz</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.breathingMax', 'Respiração Máx')}</span>
          <input
            type="range"
            min="0.30"
            max="0.80"
            step="0.05"
            value={settings.breathing_freq_max_hz ?? 0.50}
            onChange={(e) => handleChange('breathing_freq_max_hz', parseFloat(e.target.value))}
          />
          <output>{(settings.breathing_freq_max_hz ?? 0.50).toFixed(2)} Hz</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.heartMin', 'Cardíaco Mín')}</span>
          <input
            type="range"
            min="0.50"
            max="1.50"
            step="0.10"
            value={settings.heart_rate_freq_min_hz ?? 0.80}
            onChange={(e) => handleChange('heart_rate_freq_min_hz', parseFloat(e.target.value))}
          />
          <output>{(settings.heart_rate_freq_min_hz ?? 0.80).toFixed(2)} Hz</output>
        </label>

        <label className="control">
          <span className="control__label">{t('controls.heartMax', 'Cardíaco Máx')}</span>
          <input
            type="range"
            min="1.50"
            max="4.00"
            step="0.10"
            value={settings.heart_rate_freq_max_hz ?? 3.00}
            onChange={(e) => handleChange('heart_rate_freq_max_hz', parseFloat(e.target.value))}
          />
          <output>{(settings.heart_rate_freq_max_hz ?? 3.00).toFixed(2)} Hz</output>
        </label>

        {feedbackMessage && (
          <p className={`controls-feedback controls-feedback--${feedbackTone ?? 'neutral'}`} aria-live="polite">
            {feedbackMessage}
          </p>
        )}
      </div>
  );

  if (inline) {
    return <div className="controls-inline">{body}</div>;
  }

  return (
    <aside className={`controls-drawer ${isOpen ? 'open' : ''}`}>
      <button className="controls-toggle" onClick={onToggle} title={t('controls.openTitle', 'Abrir controles')}>
        <Settings2 size={20} />
      </button>
      {body}
    </aside>
  );
}
