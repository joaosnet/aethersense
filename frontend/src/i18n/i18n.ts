import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import pt from './locales/pt.json';
import en from './locales/en.json';

const savedLanguage = localStorage.getItem('i18nextLng') || 'pt';

i18n
  .use(initReactI18next)
  .init({
    resources: {
      pt: { translation: pt },
      en: { translation: en },
    },
    lng: savedLanguage,
    fallbackLng: 'pt',
    interpolation: {
      escapeValue: false, // React já protege contra XSS
    },
  });

export default i18n;
